// A tiny C ABI over Detour's C++ query API, so the Rust adapter can path on vmangos's mmaps
// (the exact Recast/Detour build + version that generated them is vendored under vendor/detour/).
// We keep ALL Detour logic here and expose WoW world coordinates to Rust; the WoW<->Recast axis
// permutation (recast = {wowY, wowZ, wowX}, no sign flips) is applied internally and matches
// vmangos PathFinder.cpp exactly.
//
// Memory: a tile buffer handed to dtNavMesh::addTile must be Detour-owned and mutable (Detour
// patches internal offsets in place and frees it with dtFree on removeTile), so we copy the caller's
// bytes into a dtAlloc buffer and pass DT_TILE_FREE_DATA.

#include "DetourNavMesh.h"
#include "DetourNavMeshQuery.h"
#include "DetourStatus.h"

#include <cstdlib>
#include <cstring>

struct NavHandle {
    dtNavMesh* mesh;
    dtNavMeshQuery* query;
};

extern "C" {

// params_data = the raw bytes of a `<map>.mmap` file (a dtNavMeshParams, 28 bytes). Returns null on
// failure. The query is sized for 2048 nodes, matching vmangos (MMapManager::GetNavMeshQuery).
NavHandle* nav_create(const unsigned char* params_data, int params_size) {
    if (!params_data || params_size < (int)sizeof(dtNavMeshParams))
        return nullptr;
    dtNavMeshParams params;
    memcpy(&params, params_data, sizeof(dtNavMeshParams));

    dtNavMesh* mesh = dtAllocNavMesh();
    if (!mesh)
        return nullptr;
    if (dtStatusFailed(mesh->init(&params))) {
        dtFreeNavMesh(mesh);
        return nullptr;
    }
    dtNavMeshQuery* query = dtAllocNavMeshQuery();
    if (!query) {
        dtFreeNavMesh(mesh);
        return nullptr;
    }
    if (dtStatusFailed(query->init(mesh, 2048))) {
        dtFreeNavMeshQuery(query);
        dtFreeNavMesh(mesh);
        return nullptr;
    }
    NavHandle* h = (NavHandle*)malloc(sizeof(NavHandle));
    h->mesh = mesh;
    h->query = query;
    return h;
}

// tile_data = raw dtMeshTile bytes (the 20-byte MmapTileHeader already stripped on the Rust side).
// Detour infers the tile's grid (x,y) from the data's dtMeshHeader. Returns 0 on success.
int nav_add_tile(NavHandle* h, const unsigned char* tile_data, int tile_size) {
    if (!h || !h->mesh || !tile_data || tile_size <= 0)
        return -1;
    unsigned char* buf = (unsigned char*)dtAlloc(tile_size, DT_ALLOC_PERM);
    if (!buf)
        return -2;
    memcpy(buf, tile_data, tile_size);
    dtTileRef ref = 0;
    dtStatus s = h->mesh->addTile(buf, tile_size, DT_TILE_FREE_DATA, 0, &ref);
    if (dtStatusFailed(s)) {
        dtFree(buf);
        return -3;
    }
    return 0;
}

void nav_destroy(NavHandle* h) {
    if (!h)
        return;
    if (h->query)
        dtFreeNavMeshQuery(h->query);
    if (h->mesh)
        dtFreeNavMesh(h->mesh);
    free(h);
}

// Find a walk path from (sx,sy,sz) to (ex,ey,ez) in WoW world coords. include_flags is the NavTerrain
// bitmask to accept (e.g. NAV_GROUND|NAV_WATER = 0x09). Writes up to max_points waypoints as WoW
// (x,y,z) triples into out_xyz (caller buffer of 3*max_points floats). Returns the number of
// waypoints, or -1 if either endpoint is off the mesh or no path exists.
int nav_find_path(NavHandle* h,
                  float sx, float sy, float sz,
                  float ex, float ey, float ez,
                  unsigned short include_flags,
                  float* out_xyz, int max_points) {
    if (!h || !h->query || !out_xyz || max_points <= 0)
        return -1;

    dtQueryFilter filter;
    filter.setIncludeFlags(include_flags);
    filter.setExcludeFlags(0);

    // WoW (x,y,z) -> Recast (y,z,x).
    float startPos[3] = { sy, sz, sx };
    float endPos[3]   = { ey, ez, ex };
    float extents[3]  = { 5.0f, 10.0f, 5.0f }; // matches vmangos PathInfo::FindWalkPoly

    dtPolyRef startRef = 0, endRef = 0;
    float startNearest[3], endNearest[3];
    if (dtStatusFailed(h->query->findNearestPoly(startPos, extents, &filter, &startRef, startNearest)) || !startRef)
        return -1;
    if (dtStatusFailed(h->query->findNearestPoly(endPos, extents, &filter, &endRef, endNearest)) || !endRef)
        return -1;

    dtPolyRef polys[256];
    int npolys = 0;
    if (dtStatusFailed(h->query->findPath(startRef, endRef, startNearest, endNearest, &filter, polys, &npolys, 256)) || npolys == 0)
        return -1;

    // String-pull to corner waypoints.
    float* straight = (float*)malloc(sizeof(float) * 3 * max_points);
    unsigned char* flags = (unsigned char*)malloc(sizeof(unsigned char) * max_points);
    dtPolyRef* spolys = (dtPolyRef*)malloc(sizeof(dtPolyRef) * max_points);
    int nstraight = 0;
    dtStatus ss = h->query->findStraightPath(startNearest, endNearest, polys, npolys,
                                             straight, flags, spolys, &nstraight, max_points);
    int result = -1;
    if (!dtStatusFailed(ss) && nstraight > 0) {
        for (int i = 0; i < nstraight; ++i) {
            // Recast (y,z,x) -> WoW (x,y,z) = (r[2], r[0], r[1]).
            out_xyz[i * 3 + 0] = straight[i * 3 + 2];
            out_xyz[i * 3 + 1] = straight[i * 3 + 0];
            out_xyz[i * 3 + 2] = straight[i * 3 + 1];
        }
        result = nstraight;
    }
    free(straight);
    free(flags);
    free(spolys);
    return result;
}

// The precise navmesh SURFACE height at WoW (x,y) near z — the detail-mesh height of the walkable
// poly under that point (terrain ground, a building floor, a stair tread, …). Returns the WoW Z, or
// -100000 if there's no walkable poly near (x,y,z). Used to place the body on the actual walkable
// surface (vs. the coarse string-pulled waypoint heights), so she sits ON floors instead of being
// buried by the terrain under them.
float nav_height(NavHandle* h, float x, float y, float z, unsigned short include_flags) {
    if (!h || !h->query)
        return -100000.0f;
    dtQueryFilter filter;
    filter.setIncludeFlags(include_flags);
    filter.setExcludeFlags(0);
    float pt[3] = { y, z, x };                 // WoW (x,y,z) -> Recast (y,z,x)
    float extents[3] = { 2.0f, 4.0f, 2.0f };   // tight, and the z hint picks the right vertical level
    dtPolyRef ref = 0;
    float nearest[3];
    if (dtStatusFailed(h->query->findNearestPoly(pt, extents, &filter, &ref, nearest)) || !ref)
        return -100000.0f;
    float height = 0.0f;
    if (dtStatusFailed(h->query->getPolyHeight(ref, nearest, &height)))
        return nearest[1];                      // fall back to the snapped point's height (recast Y)
    return height;                              // recast Y == WoW Z
}

} // extern "C"
