// Phase-0 toolchain probe: proves the src/rust workspace compiles to a
// native illumos binary, links the platform libc, and runs. Deleted once
// real tools land.

fn main() {
    println!(
        "hello-illumos: os={} arch={} ptr={}bit",
        std::env::consts::OS,
        std::env::consts::ARCH,
        usize::BITS,
    );
}
