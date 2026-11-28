#![doc = include_str!("../readme.md")]

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const RUNTIME_PKG: &str = "Microsoft.WindowsAppSDK.Runtime";
// Pair with tool-reactor-metadata's WINDOWS_APP_SDK_VERSION; that tool checks agreement.
const RUNTIME_VER: &str = "2.6.4";
// The SHA-256 of each package's .nupkg, as the feed serves it. Change it with the version above.
const RUNTIME_SHA256: &str = "3e6d1230df782973d09174cf874c947ee5fbbbe77198c52750771f71a085aa0c";
// On upgrades, check runtime.txt against the runtime payload and app.manifest against the SDK
// component packages' package.appxfragment files. These assets are not regenerated.
const RUNTIME_FILES: &str = include_str!("../assets/runtime.txt");
const APP_MANIFEST: &str = include_str!("../assets/app.manifest");
const NUGET_URL: &str = "https://www.nuget.org/api/v2/package/{name}/{version}";
const WEBVIEW2_PKG: &str = "Microsoft.Web.WebView2";
// Pair with tool-webview's WEBVIEW2_VERSION; tool-reactor-metadata checks agreement.
const WEBVIEW2_VER: &str = "1.0.4078.44";
const WEBVIEW2_SHA256: &str = "dc4d1d9168df26b830398303e50210b6e1729f6ce5a7ac69d2c766852f489962";
const WEBVIEW2_CORE_DLL: &str = "Microsoft.Web.WebView2.Core.dll";
const SELF_CONTAINED_MARKER: &str = "windows-reactor-self-contained";

fn assert_windows() {
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("windows") => {}
        Ok(os) => panic!("unsupported target OS: {os}"),
        Err(_) => panic!("CARGO_CFG_TARGET_OS not set"),
    }
}

/// Configures the app to run completely self-contained.
pub fn as_self_contained() {
    stage_self_contained().unwrap_or_else(|error| panic!("runtime staging failed: {error}"));
}

fn stage_self_contained() -> io::Result<()> {
    assert_windows();

    let out_dir = out_dir();
    let dest = target_dir_from_out(&out_dir);
    let temp_dir = temp_dir()?;
    let arch = target_arch();
    let (runtime_sha256, webview2_sha256) = pinned_sums();
    let runtime = stage_pkg(RUNTIME_PKG, RUNTIME_VER, &runtime_sha256, &temp_dir, &runtime_msix(arch))?;
    let extract = ensure_msix_extracted(runtime.path(), arch)?;
    fs::create_dir_all(&dest).map_err(|error| file_error("create output directory", &dest, error))?;
    let _destination_lock = lock_file(&dest.join(".windows-reactor-setup.lock"))?;
    copy_runtime_to(&extract, &dest)?;
    deploy_webview2(&temp_dir, &dest, arch, &webview2_sha256)?;

    let manifest_path = out_dir.join("app.manifest");
    let mut manifest = APP_MANIFEST.to_string();
    let assembly = manifest.find("<assembly").unwrap();
    let opening = assembly + manifest[assembly..].find('>').unwrap() + 1;
    manifest.insert_str(opening, &format!("<description>{SELF_CONTAINED_MARKER}</description>"));
    fs::write(&manifest_path, manifest)
        .map_err(|error| file_error("write manifest", &manifest_path, error))?;
    let target_env = env::var("CARGO_CFG_TARGET_ENV").expect("CARGO_CFG_TARGET_ENV not set");
    let target_abi = env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();
    match (target_env.as_str(), target_abi.as_str()) {
        ("msvc", _) => {
            println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest_path.display());
        }
        ("gnu", "llvm") => {
            println!("cargo:rustc-link-arg-bins=-Wl,/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bins=-Wl,/MANIFESTINPUT:{}", manifest_path.display());
        }
        _ => panic!("unsupported target environment: {target_env}{target_abi}"),
    }
    Ok(())
}

/// Deploys `Microsoft.Web.WebView2.Core.dll` next to the executable.
///
/// The XAML `WebView2` control hosted by `windows-webview`'s `reactor` feature
/// loads this WinRT projection assembly at runtime. Unlike the COM-only path
/// (`webview2loader.dll`, supplied by the Evergreen runtime), it is not present
/// on the machine by default, so a self-contained app must carry it alongside
/// the other runtime DLLs.
fn deploy_webview2(temp: &Path, dest: &Path, arch: &str, sha256: &str) -> io::Result<()> {
    let relative = PathBuf::from(format!("win-{arch}")).join("native_uap").join(WEBVIEW2_CORE_DLL);
    let pkg = stage_pkg(WEBVIEW2_PKG, WEBVIEW2_VER, sha256, temp, &relative)?;
    copy_entry(&pkg.path().join(relative), &dest.join(WEBVIEW2_CORE_DLL))
}

fn out_dir() -> PathBuf {
    PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"))
}

fn temp_dir() -> io::Result<PathBuf> {
    let base = if let Some(p) = env::var_os("LOCALAPPDATA") {
        PathBuf::from(p)
    } else if let Some(p) = env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(p)
    } else if let Some(p) = env::var_os("HOME") {
        PathBuf::from(p).join(".cache")
    } else {
        panic!("could not determine cache directory: LOCALAPPDATA, XDG_CACHE_HOME, and HOME are all unset");
    };
    // Keep caches published by this implementation separate from unchecked legacy entries.
    let temp = base.join("windows-reactor-setup").join("temp").join("v2");
    fs::create_dir_all(&temp).map_err(|error| file_error("create cache", &temp, error))?;
    Ok(temp)
}

fn runtime_msix(arch: &str) -> PathBuf {
    PathBuf::from("MSIX").join(format!("win10-{arch}")).join("Microsoft.WindowsAppRuntime.2.msix")
}

fn ensure_msix_extracted(runtime: &Path, arch: &str) -> io::Result<PathBuf> {
    cached_dir(runtime, &format!(".msix_extract-{arch}"), |partial| {
        extract_tar(&runtime.join(runtime_msix(arch)), partial, &[])?;
        require_file(&partial.join("Microsoft.WindowsAppRuntime.dll"))?;
        require_file(&partial.join("Microsoft.ui.xaml.dll"))
    })
}

fn copy_runtime_to(src: &Path, dest: &Path) -> io::Result<()> {
    for entry in fs::read_dir(src).map_err(|error| file_error("read directory", src, error))? {
        let entry = entry.map_err(|error| file_error("read directory entry", src, error))?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| RUNTIME_FILES.lines().any(|line| line.trim().eq_ignore_ascii_case(name)))
        {
            copy_entry(&entry.path(), &dest.join(name))?;
        }
    }
    Ok(())
}

fn copy_entry(src: &Path, dest: &Path) -> io::Result<()> {
    let metadata = fs::metadata(src).map_err(|error| file_error("read metadata", src, error))?;
    if metadata.is_dir() {
        fs::create_dir_all(dest).map_err(|error| file_error("create directory", dest, error))?;
        for entry in fs::read_dir(src).map_err(|error| file_error("read directory", src, error))? {
            let entry = entry.map_err(|error| file_error("read directory entry", src, error))?;
            copy_entry(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else {
        let parent = dest.parent().unwrap();
        fs::create_dir_all(parent).map_err(|error| file_error("create directory", parent, error))?;
        fs::copy(src, dest).map_err(|error| {
            io::Error::new(error.kind(), format!("copy {} to {}: {error}", src.display(), dest.display()))
        })?;
    }
    Ok(())
}

/// The SHA-256 each package has to have: the runtime's, then WebView2's. The crate's own tests name
/// others, for packages they make.
fn pinned_sums() -> (String, String) {
    #[cfg(test)]
    if let Ok(sums) = env::var("WINDOWS_REACTOR_SETUP_TEST_SUMS") {
        let (runtime, webview2) = sums.split_once(',').expect("two sums");
        return (runtime.to_owned(), webview2.to_owned());
    }
    (RUNTIME_SHA256.to_owned(), WEBVIEW2_SHA256.to_owned())
}

/// A package unpacked for one run, and removed again when it goes out of use.
struct Staged(PathBuf);

impl Staged {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Unpacks the package `name` at `ver` into a folder of this run's own. The .nupkg it comes from is
/// kept in the cache, and has the SHA-256 `sha256` each time it is used.
fn stage_pkg(name: &str, ver: &str, sha256: &str, temp: &Path, required: &Path) -> io::Result<Staged> {
    let nupkg = verified_nupkg(name, ver, sha256, temp)?;
    let dest = temp.join(format!("{name}-{ver}.{}", std::process::id()));
    if dest.try_exists().map_err(|error| file_error("read staging metadata", &dest, error))? {
        fs::remove_dir_all(&dest)
            .map_err(|error| file_error("remove old staging directory", &dest, error))?;
    }
    fs::create_dir(&dest).map_err(|error| file_error("create staging directory", &dest, error))?;
    let staged = Staged(dest);
    extract_tar(&nupkg, staged.path(), &["--strip-components=1"])?;
    require_file(&staged.path().join(required))?;
    Ok(staged)
}

/// The cached .nupkg of the package, downloaded first if it isn't there or isn't the one with the
/// SHA-256 `sha256`. A download that isn't that file is removed and is an error.
fn verified_nupkg(name: &str, ver: &str, sha256: &str, temp: &Path) -> io::Result<PathBuf> {
    let nupkg = temp.join(format!("{name}-{ver}.nupkg"));
    let _lock = lock_file(&temp.join(format!("{name}-{ver}.nupkg.lock")))?;
    if nupkg.try_exists().map_err(|error| file_error("read cache metadata", &nupkg, error))? {
        if file_sha256(&nupkg)? == sha256 {
            return Ok(nupkg);
        }
        fs::remove_file(&nupkg).map_err(|error| file_error("remove changed package", &nupkg, error))?;
    }
    let partial = temp.join(format!("{name}-{ver}.nupkg.partial"));
    dl_nupkg(name, ver, &partial)?;
    let found = file_sha256(&partial)?;
    if found != sha256 {
        let _ = fs::remove_file(&partial);
        return Err(io::Error::other(format!(
            "{name} {ver} has SHA-256 {found}, not the {sha256} this build names"
        )));
    }
    fs::rename(&partial, &nupkg).map_err(|error| file_error("publish package", &nupkg, error))?;
    Ok(nupkg)
}

/// The SHA-256 of a file, in lower case, from the system's own tool: certutil on Windows, sha256sum
/// where the crate's tests run.
fn file_sha256(path: &Path) -> io::Result<String> {
    let output = if cfg!(windows) {
        Command::new(system_tool("certutil.exe")?).arg("-hashfile").arg(path).arg("SHA256").output()
    } else {
        Command::new("sha256sum").arg(path).output()
    }
    .map_err(|error| file_error("hash", path, error))?;
    if !output.status.success() {
        return Err(io::Error::other(format!("hashing {} failed with {}", path.display(), output.status)));
    }
    sha256_in(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        io::Error::other(format!("no SHA-256 in what the hash tool said of {}", path.display()))
    })
}

/// The first 64 hex digits in the tool's output: certutil puts them on a line of their own, some
/// versions with spaces between the bytes, and sha256sum puts them first on its line.
fn sha256_in(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let first = line.split_whitespace().next().unwrap_or("");
        let digits: String =
            if first.len() == 64 { first.to_owned() } else { line.split_whitespace().collect() };
        (digits.len() == 64 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then(|| digits.to_ascii_lowercase())
    })
}

fn cached_dir(
    root: &Path,
    name: &str,
    populate: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let lock_path = root.join(format!("{name}.lock"));
    let _lock = lock_file(&lock_path)?;
    let dest = root.join(name);
    match fs::metadata(&dest) {
        Ok(metadata) if metadata.is_dir() => return Ok(dest),
        Ok(_) => {
            return Err(io::Error::other(format!("not a directory: {}", dest.display())));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(file_error("read cache metadata", &dest, error)),
    }

    let partial = root.join(format!("{name}.partial"));
    if partial.try_exists().map_err(|error| file_error("read incomplete cache metadata", &partial, error))? {
        fs::remove_dir_all(&partial)
            .map_err(|error| file_error("remove incomplete cache", &partial, error))?;
    }
    fs::create_dir(&partial).map_err(|error| file_error("create staging directory", &partial, error))?;
    populate(&partial)?;
    fs::rename(&partial, &dest).map_err(|error| file_error("publish cache", &dest, error))?;
    Ok(dest)
}

fn lock_file(path: &Path) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|error| file_error("open lock", path, error))?;
    file.lock().map_err(|error| file_error("lock", path, error))?;
    Ok(file)
}

fn require_file(path: &Path) -> io::Result<()> {
    if fs::metadata(path).map_err(|error| file_error("read required file", path, error))?.is_file() {
        Ok(())
    } else {
        Err(io::Error::other(format!("not a file: {}", path.display())))
    }
}

fn file_error(action: &str, path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{action} {}: {error}", path.display()))
}

fn dl_nupkg(name: &str, ver: &str, dest: &Path) -> io::Result<()> {
    let url = NUGET_URL.replace("{name}", name).replace("{version}", ver);
    println!("Downloading {name} {ver}");
    run(Command::new(system_tool("curl.exe")?)
        .args(["--fail", "--silent", "--show-error", "--location", "--output"])
        .arg(dest)
        .arg(url))
}

fn extract_tar(src: &Path, dst: &Path, extra: &[&str]) -> io::Result<()> {
    println!("Extracting {} to {}", src.display(), dst.display());
    run(Command::new(system_tool("tar.exe")?).arg("-xf").arg(src).arg("-C").arg(dst).args(extra))
}

fn system_tool(name: &str) -> io::Result<PathBuf> {
    env::var_os("SystemRoot")
        .map(|root| PathBuf::from(root).join("System32").join(name))
        .ok_or_else(|| io::Error::other("SystemRoot is not set"))
}

fn run(command: &mut Command) -> io::Result<()> {
    let output = command
        .output()
        .map_err(|error| io::Error::new(error.kind(), format!("run {command:?}: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{command:?} failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn target_dir_from_out(out: &Path) -> PathBuf {
    target_dir_for_out(out).unwrap_or_else(|| panic!("unsupported Cargo OUT_DIR: {}", out.display()))
}

// PROFILE reports debug or release even for custom profiles. Find the profile directory from
// `<profile>/build/<package>-<hash>/out` or `<profile>/build/<package>/<hash>/out` instead.
fn target_dir_for_out(out: &Path) -> Option<PathBuf> {
    if out.file_name() != Some(OsStr::new("out")) {
        return None;
    }
    [3, 2]
        .into_iter()
        .filter_map(|index| out.ancestors().nth(index))
        .find(|path| path.file_name() == Some(OsStr::new("build")))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

fn target_arch() -> &'static str {
    match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            loop {
                let path = env::temp_dir().join(format!(
                    "windows-reactor-setup-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("{error}"),
                }
            }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[cfg(windows)]
    fn archive(src: &Path, dest: &Path) {
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        run(Command::new(system_tool("tar.exe").unwrap())
            .args(["--format=zip", "-cf"])
            .arg(dest)
            .arg("-C")
            .arg(src)
            .arg("."))
        .unwrap();
    }

    #[cfg(windows)]
    fn archive_entries(src: &Path, dest: &Path, entries: &[&str]) {
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        run(Command::new(system_tool("tar.exe").unwrap())
            .args(["--format=zip", "-cf"])
            .arg(dest)
            .arg("-C")
            .arg(src)
            .args(entries))
        .unwrap();
    }

    #[cfg(windows)]
    fn runtime_archive(root: &Path, arch: &str) {
        let payload = root.join(format!("payload-{arch}"));
        write(&payload.join("Microsoft.WindowsAppRuntime.dll"), arch);
        write(&payload.join("Microsoft.ui.xaml.dll"), arch);
        archive(&payload, &root.join(runtime_msix(arch)));
    }

    #[test]
    #[cfg(windows)]
    fn runtime_caches_are_architecture_specific() {
        let temp = TempDir::new();
        runtime_archive(&temp.0, "x64");
        runtime_archive(&temp.0, "x86");
        runtime_archive(&temp.0, "arm64");
        write(&temp.0.join(".msix_extract").join("Microsoft.ui.xaml.dll"), "legacy");

        let x64 = ensure_msix_extracted(&temp.0, "x64").unwrap();
        let x86 = ensure_msix_extracted(&temp.0, "x86").unwrap();
        let arm64 = ensure_msix_extracted(&temp.0, "arm64").unwrap();
        assert_ne!(x64, x86);
        assert_ne!(x86, arm64);
        assert_ne!(x64, arm64);
        assert_eq!(fs::read_to_string(x64.join("Microsoft.ui.xaml.dll")).unwrap(), "x64");
        assert_eq!(fs::read_to_string(x86.join("Microsoft.ui.xaml.dll")).unwrap(), "x86");
        assert_eq!(fs::read_to_string(arm64.join("Microsoft.ui.xaml.dll")).unwrap(), "arm64");
        fs::remove_file(temp.0.join(runtime_msix("x64"))).unwrap();
        assert_eq!(ensure_msix_extracted(&temp.0, "x64").unwrap(), x64);
    }

    #[test]
    #[cfg(windows)]
    fn failed_extraction_can_be_retried() {
        let temp = TempDir::new();
        write(&temp.0.join(runtime_msix("x64")), "not an archive");
        let error = ensure_msix_extracted(&temp.0, "x64").unwrap_err();
        assert!(error.to_string().contains("tar.exe"));
        assert!(!temp.0.join(".msix_extract-x64").exists());

        runtime_archive(&temp.0, "x64");
        let extract = ensure_msix_extracted(&temp.0, "x64").unwrap();
        assert_eq!(fs::read_to_string(extract.join("Microsoft.ui.xaml.dll")).unwrap(), "x64");
    }

    #[test]
    #[cfg(windows)]
    fn extraction_requires_runtime_files_before_publication() {
        let temp = TempDir::new();
        let payload = temp.0.join("payload");
        write(&payload.join("unrelated.txt"), "not a runtime");
        archive(&payload, &temp.0.join(runtime_msix("x64")));
        let error = ensure_msix_extracted(&temp.0, "x64").unwrap_err();
        assert!(error.to_string().contains("Microsoft.WindowsAppRuntime.dll"));
        assert!(!temp.0.join(".msix_extract-x64").exists());
    }

    const ABC_SHA256: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn the_pinned_sums_are_sha256_digests() {
        for sum in [RUNTIME_SHA256, WEBVIEW2_SHA256] {
            assert_eq!(sum.len(), 64);
            assert!(sum.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
        }
    }

    #[test]
    fn a_sum_is_found_in_what_each_hash_tool_says() {
        let certutil = "SHA256 hash of x.nupkg:\r\nBA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD\r\nCertUtil: -hashfile command completed successfully.\r\n";
        assert_eq!(sha256_in(certutil).as_deref(), Some(ABC_SHA256));
        let spaced = "SHA256 hash of x:\nba 78 16 bf 8f 01 cf ea 41 41 40 de 5d ae 22 23 b0 03 61 a3 96 17 7a 9c b4 10 ff 61 f2 00 15 ad\n";
        assert_eq!(sha256_in(spaced).as_deref(), Some(ABC_SHA256));
        assert_eq!(sha256_in(&format!("{ABC_SHA256}  x.nupkg\n")).as_deref(), Some(ABC_SHA256));
        assert_eq!(sha256_in("CertUtil: -hashfile command failed\n"), None);
        assert_eq!(sha256_in(""), None);
    }

    #[test]
    fn a_cached_package_is_used_only_with_the_sum_pinned_for_it() {
        let temp = TempDir::new();
        let nupkg = temp.0.join("pkg-1.0.nupkg");
        write(&nupkg, "abc");
        assert_eq!(verified_nupkg("pkg", "1.0", ABC_SHA256, &temp.0).unwrap(), nupkg);
        assert_eq!(fs::read_to_string(&nupkg).unwrap(), "abc");
    }

    // Off Windows there is no curl.exe to fetch a replacement, so what is kept as the cache and isn't
    // the pinned file ends in an error, with the file gone.
    #[test]
    #[cfg(not(windows))]
    fn a_cached_package_with_another_sum_is_removed_and_not_used() {
        let temp = TempDir::new();
        let nupkg = temp.0.join("pkg-1.0.nupkg");
        write(&nupkg, "changed");
        verified_nupkg("pkg", "1.0", ABC_SHA256, &temp.0).unwrap_err();
        assert!(!nupkg.exists());
    }

    #[test]
    fn failed_population_is_not_reused() {
        let temp = TempDir::new();
        let error = cached_dir(&temp.0, "package", |partial| {
            write(&partial.join("incomplete.nupkg"), "partial download");
            Err(io::Error::other("interrupted"))
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "interrupted");
        assert!(!temp.0.join("package").exists());

        let cache = cached_dir(&temp.0, "package", |partial| {
            assert!(!partial.join("incomplete.nupkg").exists());
            assert!(!temp.0.join("package").exists());
            fs::write(partial.join("complete"), "ready")
        })
        .unwrap();
        assert_eq!(fs::read_to_string(cache.join("complete")).unwrap(), "ready");
        assert_eq!(cached_dir(&temp.0, "package", |_| panic!("cache was rebuilt")).unwrap(), cache);
    }

    #[test]
    fn concurrent_population_publishes_once() {
        let temp = TempDir::new();
        let starts = std::sync::Barrier::new(2);
        let calls = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..2 {
                scope.spawn(|| {
                    starts.wait();
                    let cache = cached_dir(&temp.0, "package", |partial| {
                        calls.fetch_add(1, Ordering::Relaxed);
                        fs::write(partial.join("complete"), "ready")
                    })
                    .unwrap();
                    assert_eq!(fs::read_to_string(cache.join("complete")).unwrap(), "ready");
                });
            }
        });
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[cfg(windows)]
    fn concurrent_deployment() {
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::{Duration, Instant};

        const WORKER_ROOT: &str = "WINDOWS_REACTOR_SETUP_TEST_ROOT";
        const FILE_SHARE_READ: u32 = 1;
        if let Some(root) = env::var_os(WORKER_ROOT) {
            let root = PathBuf::from(root);
            write(&root.join(format!("ready-{}", std::process::id())), "");
            let deadline = Instant::now() + Duration::from_secs(30);
            while !root.join("start").is_file() {
                assert!(Instant::now() < deadline, "deployment workers did not start");
                std::thread::sleep(Duration::from_millis(10));
            }
            for _ in 0..4 {
                as_self_contained();
            }
            return;
        }

        struct Worker(std::process::Child);
        impl Drop for Worker {
            fn drop(&mut self) {
                if self.0.try_wait().unwrap().is_none() {
                    if let Err(error) = self.0.kill()
                        && self.0.try_wait().unwrap().is_none()
                    {
                        eprintln!("failed to stop deployment worker: {error}");
                        return;
                    }
                    self.0.wait().unwrap();
                }
            }
        }

        let temp = TempDir::new();
        let cache = temp.0.join("windows-reactor-setup").join("temp").join("v2");
        let payload = "runtime".repeat(256 * 1024);
        let build = temp.0.join("packages");
        let runtime_payload = build.join("runtime-payload");
        for name in ["Microsoft.WindowsAppRuntime.dll", "Microsoft.ui.xaml.dll"] {
            write(&runtime_payload.join(name), &payload);
        }
        archive(&runtime_payload, &build.join("runtime").join("tools").join(runtime_msix("x64")));
        let runtime_nupkg = cache.join(format!("{RUNTIME_PKG}-{RUNTIME_VER}.nupkg"));
        archive_entries(&build.join("runtime"), &runtime_nupkg, &["tools"]);
        write(
            &build
                .join("webview2")
                .join("runtimes")
                .join("win-x64")
                .join("native_uap")
                .join(WEBVIEW2_CORE_DLL),
            &payload,
        );
        let webview2_nupkg = cache.join(format!("{WEBVIEW2_PKG}-{WEBVIEW2_VER}.nupkg"));
        archive_entries(&build.join("webview2"), &webview2_nupkg, &["runtimes"]);
        let sums =
            format!("{},{}", file_sha256(&runtime_nupkg).unwrap(), file_sha256(&webview2_nupkg).unwrap());
        let dest = temp.0.join("release").join("target").join("profiling");
        write(&dest.join("Microsoft.ui.xaml.dll"), "previous deployment");
        let destination_lock = lock_file(&dest.join(".windows-reactor-setup.lock")).unwrap();
        // An active writer keeps its output unavailable until it releases the destination lock.
        let busy_file = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(dest.join("Microsoft.ui.xaml.dll"))
            .unwrap();
        let mut workers = Vec::new();
        for index in 0..8 {
            let package = if index % 2 == 0 {
                PathBuf::from(format!("worker-{index}-hash"))
            } else {
                PathBuf::from(format!("worker-{index}")).join("hash")
            };
            let out = dest.join("build").join(package).join("out");
            fs::create_dir_all(&out).unwrap();
            let log = fs::File::create(temp.0.join(format!("worker-{index}.log"))).unwrap();
            workers.push(Worker(
                Command::new(env::current_exe().unwrap())
                    .args(["--exact", "tests::concurrent_deployment", "--nocapture"])
                    .env(WORKER_ROOT, &temp.0)
                    .env("LOCALAPPDATA", &temp.0)
                    .env("WINDOWS_REACTOR_SETUP_TEST_SUMS", &sums)
                    .env("OUT_DIR", out)
                    .env("PROFILE", "release")
                    .env("CARGO_CFG_TARGET_OS", "windows")
                    .env("CARGO_CFG_TARGET_ARCH", "x86_64")
                    .env("CARGO_CFG_TARGET_ENV", "msvc")
                    .stdout(log.try_clone().unwrap())
                    .stderr(log)
                    .spawn()
                    .unwrap(),
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while !workers.iter().all(|worker| temp.0.join(format!("ready-{}", worker.0.id())).is_file()) {
            assert!(Instant::now() < deadline, "deployment workers were not ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        write(&temp.0.join("start"), "");
        let blocked = Instant::now() + Duration::from_secs(1);
        while Instant::now() < blocked {
            for worker in &mut workers {
                assert!(
                    worker.0.try_wait().unwrap().is_none(),
                    "deployment did not wait for the destination lock"
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(busy_file);
        drop(destination_lock);
        for (index, worker) in workers.iter_mut().enumerate() {
            let status = loop {
                if let Some(status) = worker.0.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < deadline, "deployment worker timed out");
                std::thread::sleep(Duration::from_millis(10));
            };
            assert!(
                status.success(),
                "worker {index}: {status}\n{}",
                fs::read_to_string(temp.0.join(format!("worker-{index}.log"))).unwrap(),
            );
        }
        for name in ["Microsoft.WindowsAppRuntime.dll", "Microsoft.ui.xaml.dll", WEBVIEW2_CORE_DLL] {
            assert_eq!(fs::read_to_string(dest.join(name)).unwrap(), payload);
        }
    }

    #[test]
    fn copy_failures_are_reported() {
        let temp = TempDir::new();
        let src = temp.0.join("source");
        let dest = temp.0.join("destination");
        let name = "Microsoft.ui.xaml.dll";
        write(&src.join(name), "runtime");
        fs::create_dir_all(dest.join(name)).unwrap();
        let error = copy_runtime_to(&src, &dest).unwrap_err().to_string();
        assert!(error.contains(&src.join(name).display().to_string()));
        assert!(error.contains(&dest.join(name).display().to_string()));

        assert!(copy_runtime_to(&temp.0.join("missing"), &dest).is_err());
        assert!(copy_entry(&temp.0.join("missing"), &dest.join("file")).is_err());
    }

    #[test]
    fn copy_preserves_selected_directories_and_filters_other_files() {
        let temp = TempDir::new();
        let src = temp.0.join("source");
        let dest = temp.0.join("destination");
        write(&src.join("Microsoft.ui.xaml.dll"), "runtime");
        let resource = Path::new("Microsoft.UI.Xaml").join("nested").join("resource");
        write(&src.join(&resource), "resource");
        write(&src.join("unrelated.txt"), "excluded");
        copy_runtime_to(&src, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("Microsoft.ui.xaml.dll")).unwrap(), "runtime");
        assert_eq!(fs::read_to_string(dest.join(resource)).unwrap(), "resource");
        assert!(!dest.join("unrelated.txt").exists());
    }

    #[test]
    fn missing_tool_is_reported() {
        let temp = TempDir::new();
        let tool = temp.0.join("missing.exe");
        let error = run(&mut Command::new(&tool)).unwrap_err().to_string();
        assert!(error.contains("missing.exe"));
    }

    #[test]
    fn finds_profile_in_standard_cargo_out_dir() {
        let target = Path::new("repo").join("target").join("debug");
        let out = target.join("build").join("package-hash").join("out");
        assert_eq!(target_dir_for_out(&out), Some(target));
    }

    #[test]
    fn finds_profile_in_split_package_cargo_out_dir() {
        let target = Path::new("repo").join("target").join("debug");
        let out = target.join("build").join("package").join("hash").join("out");
        assert_eq!(target_dir_for_out(&out), Some(target));
    }

    #[test]
    fn custom_profile_ignores_release_named_ancestors() {
        let root = Path::new("release").join("target");
        for target in [root.join("profiling"), root.join("aarch64-pc-windows-msvc").join("profiling")] {
            for package in [
                PathBuf::from("package-hash"),
                Path::new("package").join("hash"),
                Path::new("build").join("hash"),
            ] {
                let out = target.join("build").join(package).join("out");
                assert_eq!(target_dir_for_out(&out), Some(target.clone()));
            }
        }
    }

    #[test]
    #[should_panic(expected = "unsupported Cargo OUT_DIR")]
    fn unrecognized_out_dir_is_rejected() {
        let out = Path::new("release").join("target").join("profiling").join("package").join("out");
        target_dir_from_out(&out);
    }
}
