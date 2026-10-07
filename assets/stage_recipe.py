# Appended to the omarchy-iso checkout's
# configs/airootfs/usr/share/omarchy-iso/orchestrator/phases_impl.py
# by `omarchy-recipe build-iso`. No-op on ISOs without a recipe payload.
#
# Requires names already present in phases_impl.py: Path, shutil, subprocess,
# info, and InstallContext (as `ctx`). Verified by omarchy-recipe's patch tests.

RECIPE_LIVE_DIR = Path("/usr/share/omarchy-iso/recipe")
RECIPE_TARGET_SUBDIR = "var/lib/omarchy/recipe"
RECIPE_BINARY_NAME = "omarchy-recipe"
RECIPE_SERVICE_NAME = "omarchy-apply-recipe.service"


def stage_recipe(ctx) -> None:
    """Copy the recipe payload onto the target and arm the first-boot applier.

    The payload (recipe.json, dotfiles.tar.zst, vendored AUR, binary, unit)
    ships on the live ISO under /usr/share/omarchy-iso/recipe/. Plain ISOs
    carry no such directory and skip silently.
    """
    src = RECIPE_LIVE_DIR
    if not src.is_dir() or not (src / "recipe.json").exists():
        return

    info("› staging machine recipe for first boot")
    dst = ctx.target / RECIPE_TARGET_SUBDIR
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(src, dst)

    # The applier IS the recipe binary: same import path as a manual import.
    bindst = ctx.target / "usr" / "local" / "bin" / RECIPE_BINARY_NAME
    bin_src = dst / RECIPE_BINARY_NAME
    if bin_src.exists():
        shutil.copy2(bin_src, bindst)
        bindst.chmod(0o755)

    unit_src = dst / RECIPE_SERVICE_NAME
    if unit_src.exists():
        unit = ctx.target / "etc" / "systemd" / "system" / RECIPE_SERVICE_NAME
        unit.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(unit_src, unit)
        subprocess.run(
            ["arch-chroot", str(ctx.target), "systemctl", "enable", RECIPE_SERVICE_NAME],
            check=True,
        )
