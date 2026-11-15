// Compile the vendored Detour (query side) + our C ABI shim into a static lib linked into the
// adapter, so path finding uses the EXACT Recast/Detour build that generated vmangos's mmaps.
// DT_POLYREF64 is baked into the vendored DetourNavMesh.h (vmangos's patch), so the on-disk tile
// layout matches without any extra define here.
fn main() {
    let detour = "vendor/detour";
    let mut b = cc::Build::new();
    b.cpp(true)
        .include(format!("{detour}/Include"))
        .file(format!("{detour}/Source/DetourAlloc.cpp"))
        .file(format!("{detour}/Source/DetourAssert.cpp"))
        .file(format!("{detour}/Source/DetourCommon.cpp"))
        .file(format!("{detour}/Source/DetourNavMesh.cpp"))
        .file(format!("{detour}/Source/DetourNavMeshQuery.cpp"))
        .file(format!("{detour}/Source/DetourNode.cpp"))
        .file("src/navshim.cpp")
        .flag_if_supported("-std=c++14")
        .warnings(false);
    b.compile("detourshim");

    println!("cargo:rerun-if-changed=src/navshim.cpp");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=vendor/detour/Source");
    println!("cargo:rerun-if-changed=vendor/detour/Include");
}
