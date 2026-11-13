use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/dualmix.ico");
    println!("cargo:rerun-if-changed=app.manifest");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let ico = manifest_dir.join("assets").join("dualmix.ico");
    let manifest = manifest_dir.join("app.manifest");
    let rc_file = out_dir.join("dualmix.rc");
    let res_file = out_dir.join("dualmix.res");

    let ver = env!("CARGO_PKG_VERSION");
    let parts: Vec<u16> = ver.split('.').filter_map(|p| p.parse().ok()).collect();
    let (maj, min, pat) = (
        parts.first().copied().unwrap_or(2),
        parts.get(1).copied().unwrap_or(4),
        parts.get(2).copied().unwrap_or(0),
    );

    let rc_content = format!(
        "1 ICON \"{ico}\"\n\
         1 24 \"{man}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION {maj},{min},{pat},0\n\
         PRODUCTVERSION {maj},{min},{pat},0\n\
         FILEOS 0x40004\n\
         FILETYPE 0x1\n\
         BEGIN\n\
           BLOCK \"StringFileInfo\"\n\
           BEGIN\n\
             BLOCK \"040904b0\"\n\
             BEGIN\n\
               VALUE \"CompanyName\", \"Marco\"\n\
               VALUE \"FileDescription\", \"DualMix\"\n\
               VALUE \"FileVersion\", \"{ver}\"\n\
               VALUE \"InternalName\", \"DualMix.exe\"\n\
               VALUE \"OriginalFilename\", \"DualMix.exe\"\n\
               VALUE \"ProductName\", \"DualMix\"\n\
               VALUE \"ProductVersion\", \"{ver}\"\n\
             END\n\
           END\n\
           BLOCK \"VarFileInfo\"\n\
           BEGIN\n\
             VALUE \"Translation\", 0x0409, 1200\n\
           END\n\
         END\n",
        ico = ico.to_string_lossy().replace('\\', "/"),
        man = manifest.to_string_lossy().replace('\\', "/"),
    );
    std::fs::write(&rc_file, rc_content).unwrap();

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        if let Some(rc_exe) = find_rc_exe() {
            let status = Command::new(rc_exe)
                .args(["/nologo", "/fo"])
                .arg(&res_file)
                .arg(&rc_file)
                .status()
                .expect("run rc.exe");
            assert!(status.success(), "rc.exe failed");
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
        } else {
            println!("cargo:warning=rc.exe not found; skipping Windows icon/version resource");
        }
    } else if let Some(windres) = find_windres() {
        let obj = out_dir.join("dualmix_res.o");
        let status = Command::new(windres)
            .args(["-O", "coff", "-i"])
            .arg(&rc_file)
            .arg("-o")
            .arg(&obj)
            .status()
            .expect("run windres");
        assert!(status.success(), "windres failed");
        println!("cargo:rustc-link-arg-bins={}", obj.display());
    }
}

fn find_rc_exe() -> Option<PathBuf> {
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86") => "x86",
        _ => "x64",
    };
    let kits = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    if let Ok(entries) = std::fs::read_dir(kits) {
        let mut versions: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.join(arch).join("rc.exe").is_file())
            .collect();
        versions.sort();
        if let Some(best) = versions.pop() {
            return Some(best.join(arch).join("rc.exe"));
        }
    }
    None
}

fn find_windres() -> Option<&'static str> {
    for c in ["x86_64-w64-mingw32-windres", "windres"] {
        if Command::new(c).arg("--version").output().is_ok() {
            return Some(c);
        }
    }
    None
}
