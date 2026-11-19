# Code Terminology

- Audience: Contributors, players, and testers
- Reference revision: `e308e7461` on `main`
- Prerequisites: None

Below are common terminology and abbreviations used in the code.

## Game Objects

### Doodad

An interactive world object such as a crop, door, mailbox, or piece of furniture.
Its phases control behavior such as growth and available interactions.

### Unit

A world actor with unit state such as stats, buffs, and combat behavior.

### NPC

Non-Playable Character.
Any enemy or friendly person in the game with a health bar.

### Mate

A summoned pet or mount.

### Slave

Internal name for vehicles like cars, carts, and ships.

### Game object model

The Rust server uses `Entity<D>` to combine a common `ObjectCore` with
kind-specific data. `WorldObject` is an enum of typed references used by
the world, visibility, and targeting code. Object data traits expose common
unit and world-object behavior.
[Porting guide](https://github.com/AAEmu/AAEmu-1.2-Rust/blob/main/rust/crates/aaemu-game/CONTRIBUTING-PORT.md)

## Game Terms

### Ability

Main class combat skill trees.

### ActAbility

Vocational skills.

### Appellations

Also known as `Titles`.

### BmPoints

Also known as `Loyalty Tokens` in most game versions.

### Dominion

Internal name for castle sieges, not guild-vs-guild fights.

### Expedition

Internal game name for what most players call a `Guild`.

## Related

- [Home](Home.md)
- [Components](Components.md)
- [Developer Notes](Developer-Notes.md)
