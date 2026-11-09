fn main() {
    println!("cargo:rerun-if-changed=src/parakeet/native.cc");
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("src/parakeet/native.cc")
        .compile("parley_parakeet_native");
}
