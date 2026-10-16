fn main() {
    // Our exported hooks must refer to themselves: without this, taking
    // their address goes through the GOT and yields the first definition in
    // the process (another preloaded interposer, e.g. Steam's overlay), and
    // handing that out as "ours" makes it call itself forever.
    println!("cargo:rustc-cdylib-link-arg=-Wl,-Bsymbolic-functions");
}
