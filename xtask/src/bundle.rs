//! `cargo xtask bundle`: assembles `target/bundle/Reno.app` from a release build of `reno-app`.
//!
//! Banners and the Dock badge need the app to run from a bundle with an identifier, so the
//! notifications brief (docs/design/2026-11-18-desktop-notifications-brief.md) ships Reno as an
//! ad-hoc signed `Reno.app`. The bundle is assembled in a temp directory beside the target and
//! renamed into place, so a half-built bundle never stays.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

pub(crate) const USAGE: &str = "usage: cargo xtask bundle [--no-build]";

const HELP: &str = "usage: cargo xtask bundle [--no-build]

Builds `reno-app` in release and assembles target/bundle/Reno.app (id dev.reno.app, ad-hoc
signed), so desktop banners and the Dock badge carry Reno's name and icon.

  --no-build   reuse an existing target/release/reno-app instead of building it

The bundle holds the app only, not `renod`: with the LaunchAgent installed the app just
kickstarts launchd, so the daemon is not copied in. A machine without the LaunchAgent cannot
start its daemon from this bundle.";

const BUNDLE_ID: &str = "dev.reno.app";
const EXECUTABLE: &str = "reno-app";
const ICON_NAME: &str = "Reno";

pub(crate) fn run(args: &[String]) -> ExitCode {
    let mut build = true;
    for arg in args {
        match arg.as_str() {
            "--no-build" => build = false,
            "-h" | "--help" => {
                println!("{HELP}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("xtask: unknown argument {other}\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    match bundle(build) {
        Ok(app) => {
            println!("bundled {}", app.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("xtask: bundle: {error}");
            ExitCode::FAILURE
        }
    }
}

fn bundle(build: bool) -> Result<PathBuf, String> {
    if !cfg!(target_os = "macos") {
        return Err("bundles are macOS only".to_owned());
    }
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("no workspace root")?
        .to_path_buf();
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    if build {
        let status = Command::new("cargo")
            .args(["build", "--release", "-p", "reno-app"])
            .current_dir(&root)
            .status()
            .map_err(|error| format!("cannot run cargo build: {error}"))?;
        if !status.success() {
            return Err("cargo build --release -p reno-app failed".to_owned());
        }
    }
    let binary = target.join("release").join(EXECUTABLE);
    if !binary.is_file() {
        return Err(format!(
            "{} not found (run without --no-build)",
            binary.display()
        ));
    }
    let icon = root.join("apps/reno-app/assets/app-icon/Reno.icns");
    let out = target.join("bundle");
    fs::create_dir_all(&out).map_err(|error| format!("creating {}: {error}", out.display()))?;
    let staged = out.join(format!(".Reno.app.building-{}", std::process::id()));
    let result = assemble(&staged, &binary, &icon, env!("CARGO_PKG_VERSION"))
        .and_then(|()| sign(&staged))
        .and_then(|()| replace(&staged, &out.join("Reno.app")));
    if result.is_err() {
        let _ = fs::remove_dir_all(&staged);
    }
    result.map(|()| out.join("Reno.app"))
}

/// The bundle layout under `app`: Info.plist, the executable (a copy), the icon.
fn assemble(app: &Path, binary: &Path, icon: &Path, version: &str) -> Result<(), String> {
    let _ = fs::remove_dir_all(app);
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    let resources = contents.join("Resources");
    for dir in [&macos, &resources] {
        fs::create_dir_all(dir).map_err(|error| format!("creating {}: {error}", dir.display()))?;
    }
    let plist = contents.join("Info.plist");
    fs::write(&plist, info_plist(version))
        .map_err(|error| format!("writing {}: {error}", plist.display()))?;
    copy(binary, &macos.join(EXECUTABLE))?;
    copy(icon, &resources.join(format!("{ICON_NAME}.icns")))
}

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    fs::copy(from, to)
        .map(drop)
        .map_err(|error| format!("copying {}: {error}", from.display()))
}

fn info_plist(version: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key>
	<string>{BUNDLE_ID}</string>
	<key>CFBundleName</key>
	<string>Reno</string>
	<key>CFBundleDisplayName</key>
	<string>Reno</string>
	<key>CFBundleExecutable</key>
	<string>{EXECUTABLE}</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleIconFile</key>
	<string>{ICON_NAME}</string>
	<key>CFBundleShortVersionString</key>
	<string>{version}</string>
	<key>CFBundleVersion</key>
	<string>{version}</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.developer-tools</string>
</dict>
</plist>
"#
    )
}

/// Ad-hoc signature, no timestamp server (the bundle is for this machine).
fn sign(app: &Path) -> Result<(), String> {
    let status = Command::new("codesign")
        .args(["--force", "--sign", "-", "--timestamp=none"])
        .arg(app)
        .status()
        .map_err(|error| format!("cannot run codesign: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "codesign failed".to_owned())
}

/// Move `staged` over `dest`: the old bundle is set aside first and restored if the move fails.
fn replace(staged: &Path, dest: &Path) -> Result<(), String> {
    let aside = dest.with_file_name(format!(".Reno.app.old-{}", std::process::id()));
    let had_old = dest.exists();
    if had_old {
        fs::rename(dest, &aside)
            .map_err(|error| format!("moving the old bundle aside: {error}"))?;
    }
    if let Err(error) = fs::rename(staged, dest) {
        if had_old {
            let _ = fs::rename(&aside, dest);
        }
        return Err(format!("moving the new bundle into place: {error}"));
    }
    if had_old {
        let _ = fs::remove_dir_all(&aside);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("reno-xtask-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_plist_names_the_app_and_its_version() {
        let plist = info_plist("1.2.3");
        for needle in [
            "<string>dev.reno.app</string>",
            "<key>CFBundleExecutable</key>\n\t<string>reno-app</string>",
            "<key>CFBundleIconFile</key>\n\t<string>Reno</string>",
            "<key>CFBundleShortVersionString</key>\n\t<string>1.2.3</string>",
            "<key>CFBundleVersion</key>\n\t<string>1.2.3</string>",
            "<key>LSMinimumSystemVersion</key>\n\t<string>13.0</string>",
            "<key>NSHighResolutionCapable</key>\n\t<true/>",
            "<string>public.app-category.developer-tools</string>",
        ] {
            assert!(plist.contains(needle), "missing {needle}");
        }
    }

    #[test]
    fn assembly_lays_out_a_copied_binary_and_the_icon() {
        let dir = scratch("assemble");
        let (binary, icon) = (dir.join("reno-app"), dir.join("Reno.icns"));
        fs::write(&binary, b"binary").unwrap();
        fs::write(&icon, b"icon").unwrap();
        let app = dir.join("Reno.app");
        assemble(&app, &binary, &icon, "0.0.0").unwrap();
        let exe = app.join("Contents/MacOS/reno-app");
        assert!(!exe.is_symlink());
        assert_eq!(fs::read(&exe).unwrap(), b"binary");
        assert_eq!(
            fs::read(app.join("Contents/Resources/Reno.icns")).unwrap(),
            b"icon"
        );
        assert!(app.join("Contents/Info.plist").is_file());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn replacing_swaps_the_bundle_and_leaves_nothing_behind() {
        let dir = scratch("replace");
        let (staged, dest) = (dir.join("new"), dir.join("Reno.app"));
        fs::create_dir_all(&staged).unwrap();
        fs::write(staged.join("marker"), "new").unwrap();
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("stale"), "old").unwrap();
        replace(&staged, &dest).unwrap();
        assert!(dest.join("marker").is_file());
        assert!(!dest.join("stale").exists());
        assert!(!staged.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().flatten().count(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}
