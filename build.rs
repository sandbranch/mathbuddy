// The git commit this binary was built from, for `mathbuddy --version`.
fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    if let Some(sha) = git(&["rev-parse", "--short", "HEAD"]) {
        let dirty =
            git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
        println!(
            "cargo:rustc-env=MATHBUDDY_COMMIT={sha}{}",
            if dirty { ", with local changes" } else { "" }
        );
    }
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
