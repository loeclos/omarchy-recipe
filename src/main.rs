mod aur;
mod cli;
mod export;
mod import;
mod iso;
mod output;
mod recipe;
mod system;
mod validate;

use clap::Parser;
use std::collections::HashSet;

fn main() {
    // argv[0] dispatch: omarchy-recipe-export etc. symlinked to this binary.
    let argv0 = std::env::args().next().unwrap_or_default();
    let invoked = argv0.rsplit('/').next().unwrap_or("");
    let alias = match invoked {
        "omarchy-recipe-export" => Some("export"),
        "omarchy-recipe-import" => Some("import"),
        "omarchy-recipe-validate" => Some("validate"),
        "omarchy-recipe-build-iso" => Some("build-iso"),
        _ => None,
    };

    let result = match alias {
        Some(sub) => {
            // Re-parse as `omarchy-recipe <sub> <rest>`.
            let mut args: Vec<String> = std::env::args().skip(1).collect();
            args.insert(0, "omarchy-recipe".into());
            args.insert(1, sub.into());
            dispatch(args)
        }
        None => dispatch(std::env::args().collect()),
    };
    if let Err(e) = result {
        output::die(&e);
    }
}

fn dispatch(args: Vec<String>) -> Result<(), String> {
    // clap handles --help/--version/invalid args itself (correct stream +
    // exit code); only real parses come back here.
    let cli = match cli::Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    crate::output::set_quiet(!cli.verbose);
    match cli.command {
        None => {
            print_help();
            Ok(())
        }
        Some(cli::Commands::Export {
            out,
            without,
            excludes,
            aur_mode,
            aur_pkgdir,
            include_secrets,
        }) => export::run(export::ExportOptions {
            out,
            without: without.into_iter().map(|s| s.as_str().to_string()).collect::<HashSet<_>>(),
            extra_excludes: excludes,
            aur_mode,
            aur_pkgdir,
            include_secrets,
        }),
        Some(cli::Commands::Import {
            bundle,
            yes,
            skip_packages,
            skip_dotfiles,
            first_boot,
            home,
            as_user,
            offline,
        }) => {
            let res = import::run(import::ImportOptions {
                bundle,
                yes,
                skip_packages,
                skip_dotfiles,
                first_boot,
                home,
                as_user,
                offline,
            });
            // First-boot contract with the systemd unit: exit 0 only on full
            // success (the unit then disables itself); anything else stays
            // enabled and retries next boot.
            res
        }
        Some(cli::Commands::Validate { bundle }) => validate::run(bundle),
        Some(cli::Commands::BuildIso {
            bundle,
            iso_checkout,
            mirror,
            aur,
            aur_pkgdir,
            dry_run,
            workdir,
        }) => iso::run(iso::BuildIsoOptions {
            bundle,
            iso_checkout,
            mirror,
            aur,
            aur_pkgdir,
            dry_run,
            workdir,
        }),
    }
}

fn print_help() {
    println!("omarchy-recipe {}", env!("CARGO_PKG_VERSION"));
    println!("Make Omarchy configs portable without dabbling in archiso manually.");
    println!();
    println!("Usage: omarchy-recipe <export|import|validate|build-iso> [...]");
    println!();
    println!("  export [--out DIR] [--without sec,...] [--exclude GLOB] [--aur-mode wifi|vendored]");
    println!("  import [DIR] [--yes] [--skip-packages] [--skip-dotfiles]");
    println!("  validate [DIR]");
    println!("  build-iso <bundle> [--mirror stable|rc|edge] [--aur wifi|vendored] [--dry-run]");
    println!();
    println!("Run 'omarchy-recipe <cmd> --help' for details.");
}
