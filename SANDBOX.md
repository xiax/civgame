# Sandbox Mode

A minimal test environment for observing entity interactions. Launches a small 5×5 chunk map (160×160 tiles, no water) with exactly one of every entity type placed near each other.

## How to launch

```
cargo run -- --sandbox
```

The window title shows **CivGame [sandbox]** so you know you're in the right mode.

## What spawns

| Entity | Position (relative to start) |
|--------|------------------------------|
| Person | Center — tile (cx, cy) |
| Wolf | 5 tiles right of person |
| Deer | 4 left, 3 up from person |
| FruitBush (Mature) | 2 right, 5 up |
| Grain (Mature) | 2 left, 5 up |
| Tree (Mature) | Center, 7 up |
| Food ×5 (ground) | 1 right, 1 up from person |
| Wood ×3 (ground) | 1 left, 1 up from person |

All entities are within roughly a 15-tile radius and visible at the default zoom level when the game starts.

## Camera controls

| Input | Action |
|-------|--------|
| W / A / S / D or Arrow keys | Pan |
| Middle-mouse drag | Pan |
| Scroll wheel | Zoom in/out |

## What to watch

**Wolf → Deer/Person chase**: The wolf hunts deer on sight (12-tile radius). If the deer is gone and the person is alone (no other humans nearby), it will target the person instead.

**Deer flee**: Deer sense wolves and persons within 8 tiles and flee in the opposite direction.

**Person gather**: The person's hunger need will drive it toward the ground food item or the nearby plants. It picks up ground items and harvests mature plants.

**Plant growth**: Plants cycle Seed → Seedling → Mature → Overripe. Overripe plants scatter seeds to adjacent tiles. All three starting plants are already Mature so you can observe harvesting immediately.

**Combat**: When the wolf reaches an adjacent tile to its target, attacks begin. Both sides have health and a cooldown timer. Death drops ground items.

**Deer graze**: When no threat is nearby, deer graze on plants — reducing plant health over time.

## Tips

- Use the **Inspector panel** (click an entity) to see its current AI state, needs, health, and inventory in real time.
- Use the **HUD** speed controls to slow down or pause the simulation if interactions are happening too fast.
- The map is purely walkable (no water tiles), so no entity will get stuck at the starting area.
- The chunk streaming system is still active — panning the camera far enough will load more terrain, but the sandbox entities stay at the origin cluster.
