use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Section {
    /// Explicit repo packages (pacman -Qeq minus Omarchy base lists)
    Packages,
    /// AUR packages
    Aur,
    /// Themes (current + custom URLs)
    Themes,
    /// Monospace font
    Font,
    /// Shell plugins (enabled state)
    Plugins,
    /// systemd user + system services
    Services,
    /// Web apps (.desktop launchers + icons)
    Webapps,
    /// Theme background images (the bulk of most bundles)
    Backgrounds,
    /// Web-app icons
    Icons,
}

impl Section {
    pub fn as_str(self) -> &'static str {
        match self {
            Section::Packages => "packages",
            Section::Aur => "aur",
            Section::Themes => "themes",
            Section::Font => "font",
            Section::Plugins => "plugins",
            Section::Services => "services",
            Section::Webapps => "webapps",
            Section::Backgrounds => "backgrounds",
            Section::Icons => "icons",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BootWhen {
    /// Ask after the bake (skips automatically when non-interactive)
    Ask,
    /// Boot the ISO in QEMU without asking
    Yes,
    /// Never boot, no prompt
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum AurMode {
    /// Install AUR packages on first boot / import with network (yay/paru)
    Wifi,
    /// Vendor prebuilt AUR packages into the bundle/ISO for fully-offline installs
    Vendored,
}

#[derive(Parser, Debug)]
#[command(
    name = "omarchy-recipe",
    version,
    about = "Make Omarchy configs portable: export/import .recipe bundles, bake custom ISOs"
)]
pub struct Cli {
    /// Show full detail (default: stages, warnings and end summary only).
    /// Without it, docker build output streams to <workdir>/iso-build.log.
    #[arg(long, short, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Export this machine as a shareable .recipe bundle
    Export {
        /// Output bundle directory (created if missing)
        #[arg(long)]
        out: Option<String>,

        /// Skip sections: packages,aur,themes,font,plugins,services,webapps,backgrounds,icons
        #[arg(long, value_delimiter = ',')]
        without: Vec<Section>,

        /// Extra tarball exclude glob (repeatable, tar --exclude syntax)
        #[arg(long = "exclude")]
        excludes: Vec<String>,

        /// How AUR packages travel: wifi (default) or vendored (prebuilt .pkgs in bundle)
        #[arg(long, value_enum, default_value = "wifi")]
        aur_mode: AurMode,

        /// Reuse prebuilt AUR .pkg.tar.zst files from this dir instead of building
        #[arg(long)]
        aur_pkgdir: Option<String>,

        /// Include secrets in the bundle (.ssh, .gnupg, .pki, *secret*, *token*).
        /// Default filters them out. Only use on trusted channels: anyone with
        /// the bundle owns these credentials.
        #[arg(long)]
        include_secrets: bool,
    },
    /// Import a .recipe bundle onto fresh Omarchy
    Import {
        /// Bundle directory (default: current dir if it holds recipe.json)
        bundle: Option<String>,

        /// Skip confirmation prompt
        #[arg(long)]
        yes: bool,

        /// Skip package installation
        #[arg(long)]
        skip_packages: bool,

        /// Skip dotfiles restore
        #[arg(long)]
        skip_dotfiles: bool,

        /// First-boot mode (run as root on the installed target)
        #[arg(long)]
        first_boot: bool,

        /// Override target home dir (first-boot: the owner's home)
        #[arg(long)]
        home: Option<String>,

        /// Run user-scoped steps as this user (first-boot)
        #[arg(long)]
        as_user: Option<String>,

        /// Offline: never touch the network (vendored AUR via pacman -U)
        #[arg(long)]
        offline: bool,
    },
    /// Validate a .recipe bundle (recipe.json + tarball hash)
    Validate {
        /// Bundle directory (default: current dir if it holds recipe.json)
        bundle: Option<String>,
    },
    /// Bake a .recipe bundle into a custom Omarchy ISO
    BuildIso {
        /// Bundle directory (default: export this machine on the spot)
        bundle: Option<String>,

        /// Reuse an omarchy-iso checkout instead of cloning
        #[arg(long)]
        iso_checkout: Option<String>,

        /// ISO channel: stable, rc or edge
        #[arg(long, default_value = "stable")]
        mirror: String,

        /// Override bundle's AUR mode
        #[arg(long, value_enum)]
        aur: Option<AurMode>,

        /// Reuse prebuilt AUR .pkg.tar.zst files from this dir
        #[arg(long)]
        aur_pkgdir: Option<String>,

        /// Generate the custom layer + patches without running docker/mkarchiso
        #[arg(long)]
        dry_run: bool,

        /// Where to stage the prepared omarchy-iso checkout (default: temp dir)
        #[arg(long)]
        workdir: Option<String>,

        /// Boot the baked ISO in QEMU for a test drive: ask (default),
        /// yes (no prompt), or no (never, no prompt)
        #[arg(long, value_enum, default_value = "ask")]
        boot: BootWhen,
    },
}
