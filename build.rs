use std::{env, fs, path::PathBuf, process::Command};

fn tool(name: &str) -> PathBuf {
    env::var_os("JAVA_HOME")
        .map(|home| PathBuf::from(home).join("bin").join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

fn main() {
    println!("cargo:rerun-if-changed=bridge/src/GatewayBridge.java");
    println!("cargo:rerun-if-changed=bridge/test/BridgeSelfTest.java");
    println!("cargo:rerun-if-changed=bridge/test/ibgateway/GWClient.java");
    println!("cargo:rerun-if-env-changed=JAVA_HOME");
    println!("cargo:rerun-if-env-changed=GATEWAY_HOME");
    let gateway = PathBuf::from(
        env::var_os("GATEWAY_HOME")
            .expect("set GATEWAY_HOME to your installed IB Gateway directory"),
    );
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let classes = out.join("agent-classes");
    fs::create_dir_all(&classes).expect("create Java output directory");
    let status = Command::new(tool("javac"))
        .args([
            "--release",
            "17",
            "-proc:none",
            "-Xlint:all",
            "-Werror",
            "-d",
        ])
        .arg(&classes)
        .arg("-classpath")
        .arg(gateway.join("jars").join("*"))
        .arg("bridge/src/GatewayBridge.java")
        .status()
        .expect("JDK 17 or newer is required; set JAVA_HOME");
    assert!(status.success(), "bridge compilation failed");
    let manifest = out.join("agent-manifest.mf");
    fs::write(&manifest, "Manifest-Version: 1.0\nPremain-Class: dev.ibkr.gateway.GatewayBridge\nCan-Redefine-Classes: false\nCan-Retransform-Classes: false\n\n")
        .expect("write agent manifest");
    let status = Command::new(tool("jar"))
        .arg("--create")
        .arg("--date=2000-01-01T00:00:00Z")
        .arg("--file")
        .arg(out.join("gateway-bridge.jar"))
        .arg("--manifest")
        .arg(&manifest)
        .arg("-C")
        .arg(&classes)
        .arg(".")
        .status()
        .expect("JDK jar tool is required");
    assert!(status.success(), "bridge packaging failed");
    let tests = out.join("test-classes");
    fs::create_dir_all(&tests).expect("create Java test output");
    let classpath =
        env::join_paths([classes, gateway.join("jars").join("*")]).expect("test classpath");
    let status = Command::new(tool("javac"))
        .args([
            "--release",
            "17",
            "-proc:none",
            "-Xlint:all",
            "-Werror",
            "-classpath",
        ])
        .arg(classpath)
        .arg("-d")
        .arg(tests)
        .arg("bridge/test/BridgeSelfTest.java")
        .arg("bridge/test/ibgateway/GWClient.java")
        .status()
        .expect("compile Java fixtures");
    assert!(status.success(), "Java fixture compilation failed");
    println!(
        "cargo:rustc-env=GATEWAYCTL_BUILD_JAVA={}",
        tool("java").display()
    );
    println!(
        "cargo:rustc-env=GATEWAYCTL_BUILD_GATEWAY={}",
        gateway.display()
    );
}
