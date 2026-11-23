//! Branding for products that ship agl under their own name.
//!
//! The `agl` binary runs with [`Brand::default`]. A product that embeds the
//! crate — a desktop app hosting the daemon in-process, or its own command
//! line program built on [`crate::cli::run`] — calls [`set`] once, first
//! thing, to choose its command name, state directory, autostart service,
//! update source, the hubs it may link to, and the helper that does computer
//! use. Protocol names, method names, `AGL_*` environment variables and file
//! names inside the state directory stay the same in every brand.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{bail, Result};

/// GitHub releases of agl itself (see update.rs for the layout).
pub const AGL_RELEASES: &str = "https://github.com/thinkany-ai/agl/releases";

/// What a product shipping agl calls itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Brand {
    /// The command name, in usage text and messages (`agl`).
    pub cli_name: String,
    /// The human name in messages (`agl`).
    pub display_name: String,
    /// The default state directory; `AGL_HOME` still wins. None: `~/.agl`.
    pub home: Option<PathBuf>,
    /// The launchd label of the autostart service (`run.agl.daemon`).
    pub service_label: String,
    /// The systemd user unit of the autostart service (`agl.service`).
    pub systemd_unit: String,
    /// An autostart service the product registers itself instead of the
    /// launchd agent or systemd unit agl writes — on macOS, one its app
    /// bundle carries and registers with `SMAppService`, so System Settings
    /// shows the app rather than the signing team. Its launchd job keeps
    /// `service_label`. None: agl's own.
    pub service: Option<ServiceBackend>,
    /// Where updates come from, laid out like GitHub releases
    /// (`<base>/latest`, `<base>/download/v<version>/<cli_name>-<os>-<arch>`).
    /// `AGL_RELEASES_BASE` overrides it. None: this build is updated some
    /// other way (with its app), and `update` / `daemon.update` refuse.
    pub releases: Option<String>,
    /// URL prefixes of the hubs this build may link to. None: any.
    pub allowed_hubs: Option<Vec<String>>,
    /// The executable that does computer use, instead of this executable
    /// started through a product launcher. It must call
    /// [`crate::computer::helper_main`]; agl starts it responsible for
    /// itself, so macOS keeps Accessibility and Screen Recording on its
    /// bundle. With a helper, one helper serves every hub (no `app_name`).
    pub computer_helper: Option<PathBuf>,
    /// The product's own signed app executable that agents and commands run
    /// under whatever `app_name` a hub asks for, so macOS names them after
    /// that app (its icon, its signing team, one name per build) rather than
    /// after a launcher bundle agl builds per name (`run.agl.app.*`, ad hoc,
    /// no icon). Its `main` must call [`crate::app::hold_from_args`] first
    /// thing. None: agl's launchers.
    pub app_launcher: Option<PathBuf>,
    /// Bundle ids computer use never operates, on top of the built-in list
    /// and `computer.blocked_apps` (a host adds its own apps here).
    pub blocked_apps: Vec<String>,
    /// The product this build is, told to hubs in the machine `hello`.
    pub product: Option<Product>,
    /// Whether computer use is on for a hub link that does not say
    /// (`HubLink::computer_use`). Off for agl: a link gets the screen only
    /// when the person turns it on. A product whose own app ships the
    /// computer-use helper may turn it on for its links.
    pub computer_use_default: bool,
    /// Whether a hub link that does not say runs its sessions' agents with
    /// their own data homes (`HubLink::isolated_agents`). Off for agl.
    pub isolated_agents_default: bool,
    /// Whether a hub link that does not say may set a session's
    /// `instructions` (`HubLink::instructions`). Off for agl.
    pub hub_instructions_default: bool,
}

/// See [`Brand::service`]. `install` also restarts a daemon the service
/// already runs, as agl's own does (a reinstall or an upgrade).
#[derive(Clone, Copy, Debug)]
pub struct ServiceBackend {
    pub install: fn() -> Result<()>,
    pub uninstall: fn() -> Result<()>,
    pub is_installed: fn() -> bool,
}

impl PartialEq for ServiceBackend {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::fn_addr_eq(self.install, other.install)
            && std::ptr::fn_addr_eq(self.uninstall, other.uninstall)
            && std::ptr::fn_addr_eq(self.is_installed, other.is_installed)
    }
}

/// A product name and version, as hubs see it (`hello.product`).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Product {
    pub name: String,
    pub version: String,
}

impl Default for Brand {
    fn default() -> Self {
        Brand {
            cli_name: "agl".into(),
            display_name: "agl".into(),
            home: None,
            service_label: "run.agl.daemon".into(),
            systemd_unit: "agl.service".into(),
            service: None,
            releases: Some(AGL_RELEASES.into()),
            allowed_hubs: None,
            computer_helper: None,
            app_launcher: None,
            blocked_apps: Vec::new(),
            product: None,
            computer_use_default: false,
            isolated_agents_default: false,
            hub_instructions_default: false,
        }
    }
}

static BRAND: OnceLock<Brand> = OnceLock::new();

/// Sets the brand for this process. Call it once, before anything else
/// uses agl; it fails when a brand was already set or read.
pub fn set(b: Brand) -> Result<()> {
    if b.cli_name.trim().is_empty() || b.display_name.trim().is_empty() {
        bail!("a brand needs a cli_name and a display_name");
    }
    if b.service_label.trim().is_empty() || b.systemd_unit.trim().is_empty() {
        bail!("a brand needs a service_label and a systemd_unit");
    }
    BRAND.set(b).map_err(|_| anyhow::anyhow!("the agl brand is already set (or was read before set)"))
}

/// The brand of this process: [`Brand::default`] unless [`set`] chose one.
pub fn get() -> &'static Brand {
    BRAND.get_or_init(Brand::default)
}

/// The version this build reports for itself: the product's when it has
/// one, else agl's.
pub fn version() -> &'static str {
    match &get().product {
        Some(p) => p.version.as_str(),
        None => crate::protocol::VERSION,
    }
}

impl Brand {
    /// Whether this build may link to `url` (normalized like the config).
    pub fn hub_allowed(&self, url: &str) -> bool {
        match &self.allowed_hubs {
            None => true,
            Some(prefixes) => {
                let url = crate::config::normalize_url(url);
                prefixes.iter().any(|p| {
                    let p = crate::config::normalize_url(p);
                    // A prefix matches at a path boundary: `https://a.com`
                    // allows `https://a.com/x`, not `https://a.com.evil`.
                    url == p || url.starts_with(&format!("{p}/"))
                })
            }
        }
    }

    /// The error for a hub this build may not link to.
    pub fn refuse_hub(&self, url: &str) -> anyhow::Error {
        anyhow::anyhow!("{} links only to its own service; {url} is not one of them", self.display_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_agl() {
        let b = Brand::default();
        assert_eq!(b.cli_name, "agl");
        assert_eq!(b.service_label, "run.agl.daemon");
        assert_eq!(b.systemd_unit, "agl.service");
        assert_eq!(b.releases.as_deref(), Some(AGL_RELEASES));
        assert!(b.home.is_none() && b.allowed_hubs.is_none() && b.computer_helper.is_none() && b.product.is_none());
        assert!(!b.computer_use_default && !b.isolated_agents_default && !b.hub_instructions_default);
        // Unit tests never set a brand: get() is the default.
        assert_eq!(get(), &Brand::default());
        // Set once: the brand was read above, so setting fails.
        assert!(set(Brand { cli_name: "x".into(), ..Brand::default() }).is_err());
    }

    #[test]
    fn hubs_match_by_prefix_at_a_path_boundary() {
        let b = Brand { allowed_hubs: Some(vec!["https://app.example.com".into(), "http://localhost:3000/api/".into()]), ..Brand::default() };
        assert!(b.hub_allowed("https://app.example.com"));
        assert!(b.hub_allowed("https://app.example.com/api/link/w/1"));
        assert!(b.hub_allowed("https://app.example.com/api/link/w/1/"));
        assert!(!b.hub_allowed("https://app.example.com.evil.net/x"));
        assert!(!b.hub_allowed("https://other.example.com/api"));
        assert!(b.hub_allowed("http://localhost:3000/api/link/w/2"));
        assert!(!b.hub_allowed("http://localhost:3000/other"));
        assert!(Brand::default().hub_allowed("https://anything.example"));
        assert!(b.refuse_hub("https://x").to_string().contains("https://x"));
    }

    #[test]
    fn a_brand_needs_names() {
        // Validation runs before the set-once check.
        assert!(set(Brand { cli_name: " ".into(), ..Brand::default() }).unwrap_err().to_string().contains("cli_name"));
        assert!(set(Brand { systemd_unit: "".into(), ..Brand::default() }).unwrap_err().to_string().contains("systemd_unit"));
    }
}
