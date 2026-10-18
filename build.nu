#!/usr/bin/env nu

# Cross-compile for Raspberry Pi Zero 2 W and optionally deploy.
#
# One-time setup (build the custom Docker image with ALSA support):
#   docker build --platform linux/amd64 -t radio-cross-aarch64 -f Dockerfile.cross-aarch64 .
#
# Then use this script:
#   nu build.nu --release            # build only
#   nu build.nu --release --deploy   # build + deploy binary and radios.txt to cala-radio

# Target aarch64-unknown-linux-gnu = Pi Zero 2 W / Pi 3 / Pi 4 (ARMv8 64-bit).
def main [
    --release (-r)  # Build in release mode
    --deploy (-d)   # Deploy binary and radios.txt to Pi after build
    --check (-c)    # Run tests and clippy for the Pi target before building
] {
    let target = "aarch64-unknown-linux-gnu"
    let profile = if $release { "release" } else { "debug" }
    let profile_flag = if $release { ["--release"] } else { [] }

    if $check {
        # Almost all the hardware code is `#[cfg(target_os = "linux")]`, so a
        # host-only `cargo clippy` lints none of it. Lint the real target.
        print "Running tests..."
        cargo test
        print $"Running clippy for ($target)..."
        cross clippy --target $target --all-targets -- -D warnings
    }

    print $"Building for ($target) \(($profile))..."
    cross build --target $target ...$profile_flag

    if $deploy {
        let binary = $"target/($target)/($profile)/radio"
        let radios = "radios.txt"
        let dest = "cala-radio:/home/dietpi/"

        if not ($radios | path exists) {
            error make { msg: $"($radios) not found — run this from the repo root" }
        }

        print $"Deploying ($binary) and ($radios) to cala-radio..."
        rsync -avz $binary $radios $dest
        print "Done!"
        print ""
        print "If the service is installed:  nu supervisor.nu restart"
        print "Otherwise run it by hand:     ./radio radios.txt"
        print "To install the service:       nu supervisor.nu install"
    }
}
