# Construction System Overhaul Plan

## 1. Background & Motivation
The current construction system relies on rigid settlement plans with disconnected 1x1 (Huts) or 2x1 (Longhouses) building footprints placed somewhat haphazardly inside geometric zones. This leads to settlements that lack historical authenticity, visual cohesion, and organic growth. Farming zones are detached from residential areas, and there is no unified approach to scaling infrastructure according to population and historical age. 

## 2. Proposed Solution

### A. Historical Era-Based Infrastructure
The settlement planner will be updated to scale infrastructure to match the population and technological era:
*   **Paleolithic/Mesolithic:** Continue using hearth-centric band camps, but introduce modular lean-tos, windbreaks, and distinct work-areas instead of scattered isolated beds. Growth remains strictly radial and temporary.
*   **Neolithic:** Introduce the **Farmstead** concept. A residential building is no longer just a hut; it is a composite lot (House + Attached Yard/Garden). Population growth spawns new interconnected farmsteads.
*   **Chalcolithic/Bronze Age:** Shift to organic village/city growth along road spines. Buildings will begin to share walls (e.g., row houses, walled courtyards). Introduce civic infrastructure strictly tied to population thresholds (e.g., Granary at pop 20, Shrine at pop 50, Market at pop 100).

### B. Integrated Farming & Footprints (Property Lots)
*   **Lots over Zones:** Replace the current `find_footprint_in_zone` with a "Property Lot" allocator. A lot is a contiguous block that contains the main building, an optional yard, outbuildings, and personal farming plots.
*   **Household Farming:** When a Neolithic or later house is built, an attached N x M field is designated specifically for that household, making fields look historically accurate (long strips or clustered gardens) rather than monolithic, perfectly square, and detached agricultural zones.

### C. Aesthetically Pleasing Organic Expansion
*   **Growth Node Algorithm:** Instead of generating static geometric zones at settlement founding, implement a "Street & Plaza" branching algorithm. The settlement expands by extending streets and attaching new lots alongside them, adapting to the terrain (avoiding steep elevations or water).
*   **Composite Buildings:** Support modular building dimensions (e.g., L-shaped, U-shaped courtyards, varying sizes) rather than hardcoded `1x1` or `2x1` rectangles, using a procedural footprint generator.

## 3. Implementation Steps

1.  **Data Model Updates (`src/simulation/settlement.rs`)**
    *   Introduce `PropertyLot`, `StreetSpine`, and `CompositeFootprint` structs.
    *   Modify `BuildIntent` in `construction.rs` to support multi-structure blueprints (e.g., House + Wall + Farm Plot).
2.  **Generative Planners**
    *   Rewrite `build_settlement_plan` to use generative growth (e.g., road-branching algorithms) instead of static `TileRect` zones.
    *   Implement era-specific layout algorithms (e.g., `build_neolithic_farmsteads`, `build_bronze_age_streets`).
3.  **Building Generators**
    *   Add logic to generate historically accurate building shapes based on culture and era (Courtyard Houses, Longhouses with attached animal pens).
    *   Update wall placement logic to allow adjacent buildings to share walls seamlessly.
4.  **Chief AI & Infrastructure Milestones (`src/simulation/construction.rs`)**
    *   Update `chief_directive_system` and `generate_candidates` to allocate civic buildings based on strict population and era milestones.
5.  **Visuals & Polish**
    *   Ensure the pathfinding and rendering systems correctly handle the new contiguous walls and integrated farm plots.
