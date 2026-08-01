# `postgis`

Requires the `postgis` PostgreSQL extension (`CREATE EXTENSION IF NOT EXISTS postgis;`) — not auto-provisioned by Pylon. A near-complete, mechanically generated wrapper over PostGIS's own function set: every Pylon name below maps to one underlying `ST_*`/`postgis_*` function (mostly just the `st_`/`postgis_` prefix stripped and lowercased — e.g. `postgis::distance(...)` calls `ST_Distance(...)`). Where the PyQL name diverges from that pattern, or the mapping isn't obvious, it's called out explicitly. PostGIS's own docs (https://postgis.net/docs/) are the authoritative reference for behavior; this table exists so you don't have to guess the PyQL-side name.

Types used below: `geometry` and `geography` are Pylon's two PostGIS point types; `box2d`/`box3d` are their bounding-box types.

## Accessors

| Function | Maps to | Description |
|---|---|---|
| `x`, `y`, `z`, `m` | `ST_X`/`Y`/`Z`/`M` | Coordinate of a point geometry. |
| `srid` | `ST_SRID` | Spatial reference ID. |
| `zmflag`, `ndims`, `hasz`, `hasm`, `coorddim` | `ST_ZMFlag` etc. | Dimensionality info. |
| `geometrytype` | `GeometryType` | Geometry type name as a string. |
| `dimension` | `ST_Dimension` | Topological dimension (0=point, 1=line, 2=polygon). |
| `npoints`, `numpoints` | `ST_NPoints`/`ST_NumPoints` | Vertex count. |
| `nrings` | `ST_NRings` | Ring count. |
| `numgeometries`, `geometryn` | `ST_NumGeometries`/`ST_GeometryN` | Component count / nth component of a collection. |
| `numinteriorrings`, `numinteriorring`, `interiorringn`, `exteriorring` | `ST_NumInteriorRings` etc. | Polygon ring accessors. |
| `numpatches`, `patchn` | `ST_NumPatches`/`ST_PatchN` | TIN patch accessors. |
| `pointn`, `startpoint`, `endpoint` | `ST_PointN` etc. | Line vertex accessors. |
| `numcurves`, `curven`, `hasarc` | `ST_NumCurves` etc. | Curved-geometry accessors. |
| `isclosed`, `isempty`, `isring`, `issimple`, `iscollection` | `ST_IsClosed` etc. | Boolean geometry-shape predicates. |
| `isvalid`, `isvalidreason` | `ST_IsValid`/`ST_IsValidReason` | OGC validity check, and a human-readable reason for invalidity. |
| `ispolygoncw`, `ispolygonccw` | `ST_IsPolygonCW`/`CCW` | Winding-order check. |
| `isvalidtrajectory` | `ST_IsValidTrajectory` | Whether an `m`-measured line is a valid trajectory. |

## Measurement

| Function | Maps to | Description |
|---|---|---|
| `length`, `length2d`, `length3d` | `ST_Length` etc. | Line length (2D, plain, or 3D). |
| `perimeter`, `perimeter2d`, `perimeter3d` | `ST_Perimeter` etc. | Polygon perimeter. |
| `area`, `area2d` | `ST_Area` | Area. |
| `distance`, `distance3d` | `ST_Distance`/`ST_3DDistance` | Distance between two geometries/geographies. |
| `distancesphere` | `ST_DistanceSphere` | Great-circle distance on a sphere. |
| `distancespheroid` | `ST_DistanceSpheroid` | Great-circle distance on a spheroid. |
| `maxdistance`, `maxdistance3d` | `ST_MaxDistance` etc. | Maximum distance between two geometries. |
| `hausdorffdistance`, `frechetdistance` | `ST_HausdorffDistance`/`ST_FrechetDistance` | Shape-similarity distance measures. |
| `closestpointofapproach`, `distancecpa`, `cpawithin` | `ST_ClosestPointOfApproach` etc. | Trajectory closest-point-of-approach analysis. |
| `azimuth` | `ST_Azimuth` | Compass bearing from one point to another. |
| `angle` | `ST_Angle` | Angle between points/lines. |
| `minimumclearance`, `minimumclearanceline` | `ST_MinimumClearance`/`Line` | Smallest distance a vertex could move without altering topology. |
| `geometricmedian` | `ST_GeometricMedian` | Point minimizing total distance to a multipoint's points. |

## Bounding box

| Function | Maps to | Description |
|---|---|---|
| `to_box2d`, `to_box3d` | `box2d`/`box3d` casts | Convert a geometry to its bounding box. |
| `postgis_getbbox` | `postgis_getbbox` | Extract a geometry's bounding box. |
| `postgis_addbbox`, `postgis_dropbbox`, `postgis_hasbbox` | `postgis_addbbox` etc. | Attach/remove/check a cached bounding box on a geometry value. |
| `makebox2d`, `makebox3d` | `ST_MakeBox2D`/`ST_3DMakeBox` | Construct a box from two corner points. |
| `expand` | `ST_Expand` | Grow a box/geometry's bounding box by a margin. |
| `xmin`/`ymin`/`zmin`/`xmax`/`ymax`/`zmax` | `ST_XMin` etc. | Bounding-box extrema (`box3d` input). |
| `combinebbox` | `ST_CombineBBox` | Union two bounding boxes. |
| `extent_agg`, `extent3d_agg` | `ST_Extent`/`ST_3DExtent` (aggregate) | Bounding box of a set of geometries. |
| `envelope`, `boundingdiagonal` | `ST_Envelope`/`ST_BoundingDiagonal` | Rectangular envelope, or its diagonal line. |
| `orientedenvelope` | `ST_OrientedEnvelope` | Minimum-area oriented bounding rectangle. |
| `minimumboundingcircle` | `ST_MinimumBoundingCircle` | Smallest enclosing circle. |

## Construction and editing

| Function | Maps to | Description |
|---|---|---|
| `makepoint`, `makepointm` | `ST_MakePoint`/`ST_MakePointM` | Construct a point from coordinates. |
| `point`, `pointz`, `pointm`, `pointzm` | `ST_Point` etc. | Construct a point with an explicit dimensionality. |
| `makeline`, `makeline_agg` | `ST_MakeLine` | Construct a line from points/an array, or as a set aggregate. |
| `makepolygon` | `ST_MakePolygon` | Construct a polygon from a ring (and optional holes). |
| `makeenvelope` | `ST_MakeEnvelope` | Construct a rectangular polygon from coordinates. |
| `linefrommultipoint`, `linefromencodedpolyline` | `ST_LineFromMultiPoint` etc. | Construct a line from a multipoint, or decode a polyline string. |
| `addpoint`, `removepoint`, `setpoint` | `ST_AddPoint` etc. | Edit a line's points. |
| `buildarea`, `polygonize`, `polygonize_agg` | `ST_BuildArea`/`ST_Polygonize` | Build polygon(s) from linework. |
| `multi`, `forcecollection` | `ST_Multi`/`ST_ForceCollection` | Promote a geometry to its multi-/collection form. |
| `collect`, `collect_agg` | `ST_Collect` | Combine geometries into a collection. |
| `collectionextract`, `collectionhomogenize` | `ST_CollectionExtract` etc. | Extract a specific type from, or homogenize, a collection. |
| `clusterintersecting`, `clusterintersecting_agg`, `clusterwithin`, `clusterwithin_agg` | `ST_ClusterIntersecting` etc. | Group an array/set of geometries into clusters. |
| `force2d`, `force3d`, `force3dz`, `force3dm`, `force4d` | `ST_Force2D` etc. | Coerce a geometry's dimensionality. |
| `forcecurve`, `forcesfs`, `curvetoline`, `linetocurve` | `ST_ForceCurve` etc. | Convert between curved and linear representations. |
| `forcepolygoncw`, `forcepolygonccw`, `forcerhr` | `ST_ForcePolygonCW` etc. | Normalize ring winding order. |
| `reverse`, `normalize` | `ST_Reverse`/`ST_Normalize` | Reverse point order, or normalize to a canonical form. |
| `scroll` | `ST_Scroll` | Change a ring's starting point. |
| `removerepeatedpoints`, `removesmallparts`, `removeirrelevantpointsforview` | `ST_RemoveRepeatedPoints` etc. | Simplification/cleanup helpers. |
| `snaptogrid`, `snap` | `ST_SnapToGrid`/`ST_Snap` | Snap coordinates to a grid, or snap one geometry to another. |
| `quantizecoordinates` | `ST_QuantizeCoordinates` | Round coordinates to a given precision. |
| `addmeasure` | `ST_AddMeasure` | Interpolate `m` values along a line. |
| `simplify`, `simplifyvw`, `simplifypreservetopology`, `simplifypolygonhull` | `ST_Simplify` etc. | Simplification algorithms (Douglas-Peucker, Visvalingam-Whyatt, topology-preserving, hull-based). |
| `chaikinsmoothing` | `ST_ChaikinSmoothing` | Chaikin's corner-cutting smoothing. |
| `seteffectivearea` | `ST_SetEffectiveArea` | Precompute simplification weights. |
| `filterbym` | `ST_FilterByM` | Drop vertices outside an `m`-value range. |
| `subdivide` | `ST_Subdivide` | Split a geometry into pieces under a vertex-count limit. |
| `segmentize` | `ST_Segmentize` | Add vertices so no segment exceeds a given length. |
| `curvetoline` see above; `delaunaytriangles`, `triangulatepolygon`, `voronoipolygons`, `voronoilines` | `ST_DelaunayTriangles` etc. | Triangulation/tessellation. |
| `concavehull`, `convexhull` | `ST_ConcaveHull`/`ST_ConvexHull` | Hull computation. |
| `node` | `ST_Node` | Node a linework's self-intersections. |
| `split` | `ST_Split` | Split a geometry by another. |
| `sharedpaths` | `ST_SharedPaths` | Linework shared by two geometries. |
| `buffer` | `ST_Buffer` | Buffer by a radius. |
| `offsetcurve` | `ST_OffsetCurve` | Offset a line by a distance. |
| `generatepoints` | `ST_GeneratePoints` | Random points within a polygon. |
| `interpolatepoint`, `lineinterpolatepoint`, `lineinterpolatepoints`, `lineinterpolatepoint3d` | `ST_LineInterpolatePoint` etc. | Point(s) at a fractional distance along a line. |
| `linesubstring` | `ST_LineSubstring` | Substring of a line between two fractions. |
| `linelocatepoint` | `ST_LineLocatePoint` | Fractional position of the closest point on a line. |
| `locatealong`, `locatebetween`, `locatebetweenelevations` | `ST_LocateAlong` etc. | Extract points/segments by `m`/elevation value. |
| `linecrossingdirection` | `ST_LineCrossingDirection` | How two lines cross. |
| `linemerge` | `ST_LineMerge` | Merge a multilinestring into linestrings where possible. |
| `points` | `ST_Points` | Extract every vertex as a multipoint. |
| `flipcoordinates` | `ST_FlipCoordinates` | Swap X/Y. |
| `affine`, `rotate`, `rotatex`, `rotatey`, `rotatez`, `translate`, `scale`, `transscale` | `ST_Affine` etc. | Affine transformations. |
| `shiftlongitude`, `wrapx` | `ST_ShiftLongitude`/`ST_WrapX` | Longitude wrapping/normalization. |
| `letters` | `ST_Letters` | Render text as geometry outlines. |

## Overlay and set operations

| Function | Maps to | Description |
|---|---|---|
| `intersection` | `ST_Intersection` | Geometric intersection. |
| `difference` | `ST_Difference` | Geometric difference. |
| `symdifference`/`symmetricdifference` | `ST_SymDifference` | Symmetric difference. |
| `union`, `union_agg`, `unaryunion` | `ST_Union` | Geometric union (pairwise, aggregate, or of one collection's own parts). |
| `coverageunion`, `coverageunion_agg` | `ST_CoverageUnion` | Union assuming input polygons form a valid coverage (faster). |
| `boundary` | `ST_Boundary` | Topological boundary. |
| `clipbybox2d` | `ST_ClipByBox2D` | Fast clip by a bounding box. |
| `reduceprecision` | `ST_ReducePrecision` | Snap to a precision grid and fix resulting topology. |
| `makevalid`, `cleangeometry` | `ST_MakeValid`/`ST_CleanGeometry` | Repair an invalid geometry. |

## Spatial relationships (predicates)

| Function | Maps to | Description |
|---|---|---|
| `contains`, `containsproperly` | `ST_Contains`/`ST_ContainsProperly` | Full/proper containment. |
| `within`, `coveredby` | `ST_Within`/`ST_CoveredBy` | Inverse of `contains`/`covers`. |
| `covers` | `ST_Covers` | Containment allowing boundary touches. |
| `intersects`, `intersects3d` | `ST_Intersects`/`ST_3DIntersects` | Any shared point. |
| `disjoint` | `ST_Disjoint` | No shared point. |
| `touches` | `ST_Touches` | Boundary-only contact. |
| `crosses` | `ST_Crosses` | Interiors cross without one containing the other. |
| `overlaps` | `ST_Overlaps` | Partial, same-dimension overlap. |
| `equals`, `orderingequals` | `ST_Equals`/`ST_OrderingEquals` | Spatial equality, or exact point-for-point equality. |
| `dwithin`, `dwithin3d`, `dfullywithin`, `dfullywithin3d` | `ST_DWithin` etc. | Within/fully-within a given distance. |
| `relate`, `relatematch` | `ST_Relate`/`ST_RelateMatch` | Raw DE-9IM relationship matrix, and pattern matching against one. |
| `closestpoint`, `closestpoint3d`, `shortestline`, `shortestline3d`, `longestline`, `longestline3d` | `ST_ClosestPoint` etc. | The point/line realizing a distance relationship. |

## Coordinate reference systems

| Function | Maps to | Description |
|---|---|---|
| `setsrid` | `ST_SetSRID` | Attach an SRID without reprojecting. |
| `transform`, `transformpipeline`, `inversetransformpipeline` | `ST_Transform` etc. | Reproject to another SRID/PROJ pipeline. |
| `postgis_transform_geometry`, `postgis_transform_pipeline_geometry` | (same) | Lower-level transform entry points. |
| `get_proj4_from_srid` | `get_proj4_from_srid` | PROJ.4 string for an SRID. |
| `postgis_srs_codes` | `postgis_srs_codes` | Known SRS codes for an authority. |
| `postgis_typmod_dims`, `postgis_typmod_srid`, `postgis_typmod_type` | (same) | Decode a column's geometry typmod. |
| `postgis_constraint_srid`, `postgis_constraint_dims` | (same) | Read a table column's SRID/dimension CHECK constraint. |

## Conversion between `geometry`/`geography`/`box2d`/`box3d`

| Function | Maps to | Description |
|---|---|---|
| `to_geometry` | `geometry(...)` cast | Convert from `box2d`/`box3d`/text/bytes/`geography`. |
| `to_geography` | `geography(...)` cast | Convert from bytes or `geometry`. |
| `geography_cmp`, `geometry_cmp`, `geometry_hash` | (same) | Low-level comparison/hash support (mostly relevant to indexing internals). |

## Input/output formats

| Function | Maps to | Format |
|---|---|---|
| `astext`/`geomfromtext`, `pointfromtext`, `linefromtext`, `polyfromtext`/`polygonfromtext`, `mlinefromtext`/`multilinestringfromtext`, `mpointfromtext`/`multipointfromtext`, `mpolyfromtext`/`multipolygonfromtext`, `geomcollfromtext`, `bdpolyfromtext`, `bdmpolyfromtext` | `ST_AsText`/`ST_GeomFromText` etc. | WKT (well-known text). |
| `asewkt` | `ST_AsEWKT` | Extended WKT (includes SRID). |
| `asbinary`/`geomfromwkb`, `pointfromwkb`, `linefromwkb`/`linestringfromwkb`, `polyfromwkb`/`polygonfromwkb`, `mpointfromwkb`/`multipointfromwkb`, `mlinefromwkb`/`multilinefromwkb`, `mpolyfromwkb`/`multipolyfromwkb`, `geomcollfromwkb` | `ST_AsBinary`/`ST_GeomFromWKB` etc. | WKB (well-known binary). |
| `asewkb`, `geomfromewkb` | `ST_AsEWKB`/`ST_GeomFromEWKB` | Extended WKB. |
| `ashexewkb` | `ST_AsHEXEWKB` | Hex-encoded extended WKB. |
| `astwkb` | `ST_AsTWKB` | "Tiny" WKB (compact binary). |
| `geogfromtext`, `geogfromwkb` | `ST_GeogFromText`/`ST_GeogFromWKB` | Parse directly to `geography`. |
| `asgml`, `geomfromgml` | `ST_AsGML`/`ST_GeomFromGML` | GML. |
| `askml`, `geomfromkml` | `ST_AsKML`/`ST_GeomFromKML` | KML. |
| `asgeojson`, `geomfromgeojson` | `ST_AsGeoJSON`/`ST_GeomFromGeoJSON` | GeoJSON (text or `json`-typed input). |
| `asmarc21`, `geomfrommarc21` | `ST_AsMARC21`/`ST_GeomFromMARC21` | MARC21/XML (library-catalog format). |
| `assvg` | `ST_AsSVG` | SVG path data. |
| `asx3d` | `ST_AsX3D` | X3D. |
| `asmvtgeom` | `ST_AsMVTGeom` | Geometry prepared for Mapbox Vector Tile encoding. |
| `asencodedpolyline`, `linefromencodedpolyline` | `ST_AsEncodedPolyline` etc. | Google's encoded-polyline format. |
| `aslatlontext` | `ST_AsLatLonText` | Human-readable lat/lon string. |
| `geohash`, `box2dfromgeohash`, `pointfromgeohash`, `geomfromgeohash` | `ST_GeoHash` etc. | Geohash encode/decode. |

## Version info

| Function | Maps to |
|---|---|
| `postgis_version`, `postgis_lib_version`, `postgis_scripts_installed`, `postgis_scripts_released`, `postgis_lib_revision`, `postgis_svn_version`, `postgis_lib_build_date`, `postgis_scripts_build_date`, `postgis_full_version` | Core PostGIS version/build metadata. |
| `postgis_geos_version`, `postgis_geos_compiled_version` | GEOS library version. |
| `postgis_proj_version`, `postgis_proj_compiled_version` | PROJ library version. |
| `postgis_liblwgeom_version` | liblwgeom version. |
| `postgis_libjson_version`, `postgis_libxml_version`, `postgis_libprotobuf_version` | Supporting library versions (JSON/XML/protobuf). |
| `postgis_wagyu_version` | Wagyu (polygon clipping) library version. |

## Operators (`op_*`)

Every binary spatial operator PostGIS defines is reachable as an `op_*` function, for use where a function call reads better than remembering an operator symbol — e.g. `postgis::op_overlaps(a, b)` is `a && b`.

| Function | Operator | Description |
|---|---|---|
| `op_neq` | `<>` | Bounding boxes differ. |
| `op_overlaps`, `op_overlaps_nd`, `op_overlaps_2d`, `op_overlaps_3d` | `&&`, `&&&`, `&&`, `&/&` | Bounding boxes overlap (2D, n-D, 2D box-vs-geometry, 3D). |
| `op_same`, `op_same_nd`, `op_same_3d` | `~=`, `~~=`, `~==` | Bounding boxes are identical. |
| `op_contains`, `op_contains_nd`, `op_contains_2d`, `op_contains_3d` | `~`, `~~`, `~`, `@>>` | Bounding box containment. |
| `op_within`, `op_within_nd`, `op_is_contained_2d`, `op_contained_3d` | `@`, `@@`, `@`, `<<@` | Inverse of the `contains` variants. |
| `op_left`, `op_right`, `op_overleft`, `op_overright` | `<<`, `>>`, `&<`, `&>` | Bounding-box left/right positioning. |
| `op_below`, `op_above`, `op_overbelow`, `op_overabove` | `<<\|`, `\|>>`, `&<\|`, `\|&>` | Bounding-box vertical positioning. |
| `op_distance_centroid`, `op_distance_centroid_nd`, `op_distance_knn` | `<->`, `<<->>`, `<->` | Centroid-distance ordering, used by k-NN index searches. |
| `op_distance_box` | `<#>` | Bounding-box distance ordering. |
| `op_distance_cpa` | `\|=\|` | Closest-point-of-approach distance ordering (trajectories). |

## Aggregates

`extent_agg`, `extent3d_agg`, `memunion_agg`, `union_agg`, `collect_agg`, `clusterintersecting_agg`, `clusterwithin_agg`, `polygonize_agg`, `makeline_agg`, `coverageunion_agg` — the set-aggregate forms of the equivalent construction/overlay functions above (documented there); each consumes a `set of geometry` instead of two positional arguments.
