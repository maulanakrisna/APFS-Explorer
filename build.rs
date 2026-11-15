fn main() {
    // Menyematkan manifest (Common Controls v6 + hak Administrator untuk akses disk fisik).
    embed_resource::compile("app.rc", embed_resource::NONE);
}
