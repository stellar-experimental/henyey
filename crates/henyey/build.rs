use std::process::Command;

fn command_stdout(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| stdout.trim().to_string())
        .unwrap_or_default()
}

fn main() {
    let commit_hash = command_stdout("git", &["rev-parse", "HEAD"]);
    let build_timestamp = command_stdout("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]);

    println!("cargo:rustc-env=HENYEY_COMMIT_HASH={}", commit_hash);
    println!("cargo:rustc-env=HENYEY_BUILD_TIMESTAMP={}", build_timestamp);

    // Re-run if git HEAD changes
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/");
}
