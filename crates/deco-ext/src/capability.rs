//! The capability model that controls what an extension can do on the machine.
//!
//! # Why this exists
//!
//! A VS Code extension is arbitrary JavaScript running in a Node process with
//! the user's full privileges. It can read `~/.ssh/id_ed25519`, open a socket,
//! and spawn a shell. The extension API neither shows nor prevents this.
//! Installing an extension means trusting its author and every dependency in
//! its `node_modules` with everything the user can access.
//!
//! deco still runs extensions in a separate Node process, because extensions
//! are JavaScript. The host process starts with no direct filesystem, network
//! or process access. Every such operation is an RPC to deco, and deco checks
//! it here.
//!
//! # The rules
//!
//! 1. **Deny by default.** A capability that is not declared in the extension's
//!    manifest is denied and is never offered to the user. An extension cannot
//!    obtain consent at request time for a capability it did not declare.
//! 2. **Declaration is an upper bound, not a grant.** A declared capability
//!    still needs a decision: remembered, prompted for, or denied by policy.
//! 3. **Scopes are checked on the resolved path**, after `..` is collapsed, so
//!    a path cannot use `..` to leave the `workspace` scope.
//!
//! What this does *not* defend against is documented on [`Broker::check`].

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a filesystem capability may reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PathScope {
    /// Anywhere inside the open workspace folders.
    Workspace,
    /// The extension's own private storage directory.
    ExtensionStorage,
    /// The extension's own installation directory (read-only in practice).
    ExtensionInstall,
    /// A specific subtree the user named.
    Subtree {
        /// The root of the subtree.
        path: PathBuf,
    },
}

/// Something an extension may ask deco to do on its behalf.
///
/// Each variant carries its own bound. For example, a grant is never "network
/// access" but always "network access to these hosts".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "capability")]
pub enum Capability {
    /// Read files within `scope`.
    ReadFile {
        /// The reachable subtree.
        scope: PathScope,
    },
    /// Create, modify or delete files within `scope`.
    WriteFile {
        /// The reachable subtree.
        scope: PathScope,
    },
    /// Open outbound connections to `host`.
    Network {
        /// A hostname, or `*.example.com` for a subdomain wildcard.
        host: String,
    },
    /// Execute `program`.
    Process {
        /// The program name or absolute path.
        program: String,
    },
    /// Read the environment variable `name`.
    ///
    /// The host process starts with a scrubbed environment. This capability is
    /// the only way a variable from the user's environment reaches an extension.
    Env {
        /// The variable name.
        name: String,
    },
    /// Read and write the system clipboard.
    Clipboard,
    /// Store and retrieve secrets in the user's credential store.
    Secrets,
    /// Open a URI in the user's browser.
    OpenExternal,
}

impl Capability {
    /// A short, stable identifier for the *kind* of capability, used for
    /// grouping in the consent UI and for remembering decisions.
    pub fn kind(&self) -> &'static str {
        match self {
            Capability::ReadFile { .. } => "readFile",
            Capability::WriteFile { .. } => "writeFile",
            Capability::Network { .. } => "network",
            Capability::Process { .. } => "process",
            Capability::Env { .. } => "env",
            Capability::Clipboard => "clipboard",
            Capability::Secrets => "secrets",
            Capability::OpenExternal => "openExternal",
        }
    }

    /// Whether a grant of `self` covers `request`.
    ///
    /// Write access implies read access to the same scope, as in ordinary
    /// filesystems. No other capability implies another.
    pub fn covers(&self, request: &Capability, ctx: &ResolutionContext) -> bool {
        match (self, request) {
            (Capability::ReadFile { scope: granted }, Capability::ReadFile { scope: wanted })
            | (Capability::WriteFile { scope: granted }, Capability::WriteFile { scope: wanted })
            | (Capability::WriteFile { scope: granted }, Capability::ReadFile { scope: wanted }) => {
                scope_covers(granted, wanted, ctx)
            }
            (Capability::Network { host: granted }, Capability::Network { host: wanted }) => {
                host_matches(granted, wanted)
            }
            (Capability::Process { program: granted }, Capability::Process { program: wanted }) => {
                granted == wanted
            }
            (Capability::Env { name: granted }, Capability::Env { name: wanted }) => {
                granted == wanted
            }
            (Capability::Clipboard, Capability::Clipboard)
            | (Capability::Secrets, Capability::Secrets)
            | (Capability::OpenExternal, Capability::OpenExternal) => true,
            _ => false,
        }
    }
}

/// Whether `granted` contains `wanted` once both are resolved to real paths.
fn scope_covers(granted: &PathScope, wanted: &PathScope, ctx: &ResolutionContext) -> bool {
    match (granted, wanted) {
        (PathScope::Workspace, PathScope::Workspace)
        | (PathScope::ExtensionStorage, PathScope::ExtensionStorage)
        | (PathScope::ExtensionInstall, PathScope::ExtensionInstall) => true,
        (PathScope::Subtree { path: granted }, PathScope::Subtree { path: wanted }) => {
            is_within(wanted, granted)
        }
        // A named scope covers a subtree request only if the subtree is inside
        // one of that scope's roots.
        (broad, PathScope::Subtree { path: wanted }) => ctx
            .roots_for(broad)
            .iter()
            .any(|root| is_within(wanted, root)),
        _ => false,
    }
}

/// Hostname matching with a single leading-wildcard form.
///
/// `*.example.com` covers `api.example.com` but not `example.com` itself, and
/// not `notexample.com`. The dot check prevents the second case, which a plain
/// suffix comparison would accept.
fn host_matches(granted: &str, wanted: &str) -> bool {
    let granted = granted.trim().to_ascii_lowercase();
    let wanted = wanted.trim().to_ascii_lowercase();
    if let Some(suffix) = granted.strip_prefix("*.") {
        return wanted.len() > suffix.len() + 1
            && wanted.ends_with(suffix)
            && wanted.as_bytes()[wanted.len() - suffix.len() - 1] == b'.';
    }
    granted == wanted
}

/// Collapses `.` and `..` without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Whether `path` is `root` or sits inside it, comparing whole components.
///
/// Comparing components prevents `/workspace-secrets` from matching as a child
/// of `/workspace`.
pub fn is_within(path: &Path, root: &Path) -> bool {
    let path = normalize(path);
    let root = normalize(root);
    path.starts_with(&root)
}

/// The concrete directories the named scopes resolve to right now.
#[derive(Debug, Clone, Default)]
pub struct ResolutionContext {
    /// The open workspace folders.
    pub workspace_roots: Vec<PathBuf>,
    /// The extension's private storage directory.
    pub extension_storage: Option<PathBuf>,
    /// The extension's installation directory.
    pub extension_install: Option<PathBuf>,
}

impl ResolutionContext {
    /// The roots a scope currently resolves to.
    pub fn roots_for(&self, scope: &PathScope) -> Vec<PathBuf> {
        match scope {
            PathScope::Workspace => self.workspace_roots.clone(),
            PathScope::ExtensionStorage => self.extension_storage.iter().cloned().collect(),
            PathScope::ExtensionInstall => self.extension_install.iter().cloned().collect(),
            PathScope::Subtree { path } => vec![path.clone()],
        }
    }
}

/// What the user (or policy) decided about a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Permitted.
    Allow,
    /// Refused.
    Deny,
}

/// What to do about a declared capability with no remembered decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultPolicy {
    /// Ask the user once, then remember.
    #[default]
    Prompt,
    /// Deny without asking. Intended for shared machines and CI, where no user
    /// is present to answer a prompt.
    Deny,
    /// Allow anything the manifest declared without asking. This weakens the
    /// model: declaration becomes the only check.
    Allow,
}

/// The setting that controls what happens to a declared capability that has no
/// decision yet.
pub const DEFAULT_POLICY_KEY: &str = "extensions.permissions.default";

impl DefaultPolicy {
    /// Parses the `extensions.permissions.default` setting.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "prompt" => Some(DefaultPolicy::Prompt),
            "deny" => Some(DefaultPolicy::Deny),
            "allow" => Some(DefaultPolicy::Allow),
            _ => None,
        }
    }
}

/// The outcome of a capability check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckResult {
    /// The extension may proceed.
    Allowed,
    /// The extension may not proceed.
    Denied {
        /// Why, in terms the user can act on.
        reason: DenyReason,
    },
    /// The user must be asked before the operation can proceed.
    NeedsConsent {
        /// The capability to ask about.
        capability: Capability,
    },
}

/// Why a capability check failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DenyReason {
    /// The manifest never declared anything covering the request.
    #[error("the extension's manifest does not declare this capability")]
    Undeclared,
    /// The user previously refused.
    #[error("the user denied this capability")]
    UserDenied,
    /// Policy refuses without asking.
    #[error("`extensions.permissions.default` is set to deny")]
    PolicyDenied,
    /// The path resolved outside every granted scope.
    #[error("the path is outside every granted scope")]
    OutsideScope,
}

/// Remembered decisions for one extension.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrantStore {
    /// Capabilities the user has allowed.
    #[serde(default)]
    pub allowed: Vec<Capability>,
    /// Capabilities the user has refused.
    #[serde(default)]
    pub denied: Vec<Capability>,
}

/// Checks capability requests for a single extension.
#[derive(Debug, Clone)]
pub struct Broker {
    declared: Vec<Capability>,
    grants: GrantStore,
    policy: DefaultPolicy,
    context: ResolutionContext,
}

impl Broker {
    /// Builds a broker for an extension.
    pub fn new(
        declared: Vec<Capability>,
        grants: GrantStore,
        policy: DefaultPolicy,
        context: ResolutionContext,
    ) -> Self {
        Self {
            declared,
            grants,
            policy,
            context,
        }
    }

    /// The capabilities the manifest declared.
    pub fn declared(&self) -> &[Capability] {
        &self.declared
    }

    /// The remembered decisions.
    pub fn grants(&self) -> &GrantStore {
        &self.grants
    }

    /// Records the user's answer to a consent prompt.
    pub fn remember(&mut self, capability: Capability, decision: Decision) {
        match decision {
            Decision::Allow => {
                self.grants.denied.retain(|c| *c != capability);
                if !self.grants.allowed.contains(&capability) {
                    self.grants.allowed.push(capability);
                }
            }
            Decision::Deny => {
                self.grants.allowed.retain(|c| *c != capability);
                if !self.grants.denied.contains(&capability) {
                    self.grants.denied.push(capability);
                }
            }
        }
    }

    /// Forgets a decision, so the next request covered by it asks again.
    ///
    /// Removes the capability from both lists, so no earlier answer remains.
    pub fn forget(&mut self, capability: &Capability) {
        self.grants.allowed.retain(|c| c != capability);
        self.grants.denied.retain(|c| c != capability);
    }

    /// Decides what happens to `request`.
    ///
    /// # What this does not protect against
    ///
    /// Path scopes are enforced lexically. A symlink inside the workspace that
    /// points outside it will pass this check, so callers that touch the
    /// filesystem must re-verify with the resolved real path
    /// ([`Broker::check_resolved_path`] does this) before opening anything.
    /// This check also does not limit how much CPU, memory or time the
    /// extension consumes. The host process limits those separately.
    pub fn check(&self, request: &Capability) -> CheckResult {
        // Check denials first. An explicit denial takes precedence over
        // everything, including a broader allow granted earlier.
        if self
            .grants
            .denied
            .iter()
            .any(|g| g.covers(request, &self.context))
        {
            return CheckResult::Denied {
                reason: DenyReason::UserDenied,
            };
        }

        // Declaration is the upper bound. It is checked before grants, so a
        // stale grant for a capability the extension has since removed from
        // its manifest cannot be used.
        if !self
            .declared
            .iter()
            .any(|d| d.covers(request, &self.context))
        {
            return CheckResult::Denied {
                reason: DenyReason::Undeclared,
            };
        }

        if self
            .grants
            .allowed
            .iter()
            .any(|g| g.covers(request, &self.context))
        {
            return CheckResult::Allowed;
        }

        match self.policy {
            DefaultPolicy::Allow => CheckResult::Allowed,
            DefaultPolicy::Deny => CheckResult::Denied {
                reason: DenyReason::PolicyDenied,
            },
            DefaultPolicy::Prompt => CheckResult::NeedsConsent {
                capability: request.clone(),
            },
        }
    }

    /// Checks a filesystem request against the path it actually resolves to.
    ///
    /// `real_path` must be the canonicalised path. The caller supplies it
    /// because canonicalisation needs the filesystem, and this crate performs
    /// no I/O so it can be tested exhaustively. The parameter name makes it
    /// clear that passing the *unresolved* path is a mistake.
    pub fn check_resolved_path(&self, write: bool, real_path: &Path) -> CheckResult {
        let scope = PathScope::Subtree {
            path: normalize(real_path),
        };
        let request = if write {
            Capability::WriteFile { scope }
        } else {
            Capability::ReadFile { scope }
        };
        self.check(&request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ResolutionContext {
        ResolutionContext {
            workspace_roots: vec![PathBuf::from("/home/u/project")],
            extension_storage: Some(PathBuf::from("/home/u/.config/deco/storage/pub.ext")),
            extension_install: Some(PathBuf::from("/home/u/.config/deco/extensions/pub.ext")),
        }
    }

    fn read(path: &str) -> Capability {
        Capability::ReadFile {
            scope: PathScope::Subtree {
                path: PathBuf::from(path),
            },
        }
    }

    fn write(path: &str) -> Capability {
        Capability::WriteFile {
            scope: PathScope::Subtree {
                path: PathBuf::from(path),
            },
        }
    }

    fn broker(declared: Vec<Capability>, policy: DefaultPolicy) -> Broker {
        Broker::new(declared, GrantStore::default(), policy, ctx())
    }

    #[test]
    fn an_undeclared_capability_is_denied_without_prompting() {
        let b = broker(vec![], DefaultPolicy::Prompt);
        assert_eq!(
            b.check(&Capability::Clipboard),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn a_declared_capability_prompts_under_the_default_policy() {
        let b = broker(vec![Capability::Clipboard], DefaultPolicy::Prompt);
        assert_eq!(
            b.check(&Capability::Clipboard),
            CheckResult::NeedsConsent {
                capability: Capability::Clipboard
            }
        );
    }

    #[test]
    fn a_deny_policy_refuses_declared_capabilities_without_asking() {
        let b = broker(vec![Capability::Clipboard], DefaultPolicy::Deny);
        assert_eq!(
            b.check(&Capability::Clipboard),
            CheckResult::Denied {
                reason: DenyReason::PolicyDenied
            }
        );
    }

    #[test]
    fn an_allow_policy_still_requires_declaration() {
        let b = broker(vec![Capability::Clipboard], DefaultPolicy::Allow);
        assert_eq!(b.check(&Capability::Clipboard), CheckResult::Allowed);
        assert_eq!(
            b.check(&Capability::Secrets),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn a_remembered_allow_stops_the_prompting() {
        let mut b = broker(vec![Capability::Clipboard], DefaultPolicy::Prompt);
        b.remember(Capability::Clipboard, Decision::Allow);
        assert_eq!(b.check(&Capability::Clipboard), CheckResult::Allowed);
    }

    #[test]
    fn a_remembered_deny_outranks_a_broader_allow() {
        let mut b = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Prompt,
        );
        b.remember(
            Capability::ReadFile {
                scope: PathScope::Workspace,
            },
            Decision::Allow,
        );
        b.remember(read("/home/u/project/.env"), Decision::Deny);

        assert_eq!(
            b.check(&read("/home/u/project/src/main.rs")),
            CheckResult::Allowed
        );
        assert_eq!(
            b.check(&read("/home/u/project/.env")),
            CheckResult::Denied {
                reason: DenyReason::UserDenied
            }
        );
    }

    #[test]
    fn remembering_a_decision_replaces_the_opposite_one() {
        let mut b = broker(vec![Capability::Clipboard], DefaultPolicy::Prompt);
        b.remember(Capability::Clipboard, Decision::Deny);
        assert!(matches!(
            b.check(&Capability::Clipboard),
            CheckResult::Denied { .. }
        ));
        b.remember(Capability::Clipboard, Decision::Allow);
        assert_eq!(b.check(&Capability::Clipboard), CheckResult::Allowed);
        assert_eq!(b.grants().denied.len(), 0);
    }

    #[test]
    fn a_grant_for_a_capability_no_longer_declared_is_ignored() {
        // The extension was updated and removed `secrets` from its manifest.
        // The stale grant must no longer apply.
        let grants = GrantStore {
            allowed: vec![Capability::Secrets],
            denied: vec![],
        };
        let b = Broker::new(
            vec![Capability::Clipboard],
            grants,
            DefaultPolicy::Allow,
            ctx(),
        );
        assert_eq!(
            b.check(&Capability::Secrets),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn workspace_scope_covers_paths_inside_the_workspace() {
        let b = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&read("/home/u/project/src/main.rs")),
            CheckResult::Allowed
        );
    }

    #[test]
    fn workspace_scope_does_not_cover_paths_outside_it() {
        let b = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&read("/home/u/.ssh/id_ed25519")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn dot_dot_cannot_walk_out_of_a_scope() {
        let b = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        for escape in [
            "/home/u/project/../.ssh/id_ed25519",
            "/home/u/project/src/../../../etc/passwd",
            "/home/u/project/./../../etc/shadow",
        ] {
            assert_eq!(
                b.check(&read(escape)),
                CheckResult::Denied {
                    reason: DenyReason::Undeclared
                },
                "{escape} escaped the workspace"
            );
        }
    }

    #[test]
    fn a_sibling_directory_with_a_shared_prefix_is_not_inside_the_scope() {
        // `/home/u/project-secrets` must not match as a child of
        // `/home/u/project`. A plain string prefix test would accept it.
        let b = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&read("/home/u/project-secrets/keys.txt")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn write_access_implies_read_access_but_not_the_reverse() {
        let b = broker(
            vec![Capability::WriteFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&read("/home/u/project/a.txt")),
            CheckResult::Allowed
        );

        let b = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&write("/home/u/project/a.txt")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn extension_storage_is_its_own_scope() {
        let b = broker(
            vec![Capability::WriteFile {
                scope: PathScope::ExtensionStorage,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&write("/home/u/.config/deco/storage/pub.ext/cache.json")),
            CheckResult::Allowed
        );
        assert_eq!(
            b.check(&write("/home/u/project/a.txt")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn a_named_subtree_grant_covers_its_children_only() {
        let b = broker(vec![read("/opt/data")], DefaultPolicy::Allow);
        assert_eq!(b.check(&read("/opt/data/x/y.txt")), CheckResult::Allowed);
        assert_eq!(
            b.check(&read("/opt/other/x.txt")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn network_grants_are_per_host() {
        let b = broker(
            vec![Capability::Network {
                host: "api.example.com".into(),
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&Capability::Network {
                host: "api.example.com".into()
            }),
            CheckResult::Allowed
        );
        assert_eq!(
            b.check(&Capability::Network {
                host: "evil.example.com".into()
            }),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn network_host_matching_is_case_insensitive() {
        assert!(host_matches("API.Example.COM", "api.example.com"));
    }

    #[test]
    fn a_wildcard_host_covers_subdomains_only() {
        assert!(host_matches("*.example.com", "api.example.com"));
        assert!(host_matches("*.example.com", "a.b.example.com"));
        // The bare domain is not a subdomain of itself.
        assert!(!host_matches("*.example.com", "example.com"));
        // And a suffix match without the dot must not pass.
        assert!(!host_matches("*.example.com", "notexample.com"));
        assert!(!host_matches("*.example.com", "example.com.evil.net"));
    }

    #[test]
    fn process_grants_are_per_program() {
        let b = broker(
            vec![Capability::Process {
                program: "rustfmt".into(),
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&Capability::Process {
                program: "rustfmt".into()
            }),
            CheckResult::Allowed
        );
        assert_eq!(
            b.check(&Capability::Process {
                program: "sh".into()
            }),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn env_grants_are_per_variable() {
        let b = broker(
            vec![Capability::Env {
                name: "CARGO_HOME".into(),
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check(&Capability::Env {
                name: "CARGO_HOME".into()
            }),
            CheckResult::Allowed
        );
        // A grant for one variable must not expose a common exfiltration target.
        assert_eq!(
            b.check(&Capability::Env {
                name: "AWS_SECRET_ACCESS_KEY".into()
            }),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn capability_kinds_never_cross() {
        let b = broker(vec![Capability::Clipboard], DefaultPolicy::Allow);
        assert_eq!(
            b.check(&Capability::Secrets),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
        assert_eq!(
            b.check(&Capability::OpenExternal),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn check_resolved_path_normalises_before_deciding() {
        let b = broker(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            DefaultPolicy::Allow,
        );
        assert_eq!(
            b.check_resolved_path(false, Path::new("/home/u/project/src/../lib.rs")),
            CheckResult::Allowed
        );
        assert_eq!(
            b.check_resolved_path(false, Path::new("/home/u/project/../secrets")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn a_workspace_with_no_folders_grants_nothing() {
        let b = Broker::new(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            GrantStore::default(),
            DefaultPolicy::Allow,
            ResolutionContext::default(),
        );
        assert_eq!(
            b.check(&read("/anything")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn multi_root_workspaces_cover_every_folder() {
        let context = ResolutionContext {
            workspace_roots: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            ..Default::default()
        };
        let b = Broker::new(
            vec![Capability::ReadFile {
                scope: PathScope::Workspace,
            }],
            GrantStore::default(),
            DefaultPolicy::Allow,
            context,
        );
        assert_eq!(b.check(&read("/a/x")), CheckResult::Allowed);
        assert_eq!(b.check(&read("/b/y")), CheckResult::Allowed);
        assert_eq!(
            b.check(&read("/c/z")),
            CheckResult::Denied {
                reason: DenyReason::Undeclared
            }
        );
    }

    #[test]
    fn normalisation_leaves_leading_parent_components_alone() {
        // There is nothing to pop, so the `..` stays. Dropping it would turn a
        // relative escape into a path that looks harmless.
        assert_eq!(
            normalize(Path::new("../etc/passwd")),
            PathBuf::from("../etc/passwd")
        );
        assert_eq!(normalize(Path::new("a/../../b")), PathBuf::from("../b"));
    }

    #[test]
    fn default_policy_parses_the_setting_values() {
        assert_eq!(DefaultPolicy::parse("prompt"), Some(DefaultPolicy::Prompt));
        assert_eq!(DefaultPolicy::parse("deny"), Some(DefaultPolicy::Deny));
        assert_eq!(DefaultPolicy::parse("allow"), Some(DefaultPolicy::Allow));
        assert_eq!(DefaultPolicy::parse("maybe"), None);
        assert_eq!(DefaultPolicy::default(), DefaultPolicy::Prompt);
    }

    #[test]
    fn capabilities_round_trip_through_serde() {
        let caps = vec![
            Capability::ReadFile {
                scope: PathScope::Workspace,
            },
            Capability::WriteFile {
                scope: PathScope::Subtree {
                    path: "/tmp/x".into(),
                },
            },
            Capability::Network {
                host: "*.example.com".into(),
            },
            Capability::Env {
                name: "PATH".into(),
            },
            Capability::Clipboard,
        ];
        let json = serde_json::to_string(&caps).unwrap();
        let back: Vec<Capability> = serde_json::from_str(&json).unwrap();
        assert_eq!(caps, back);
    }
}
