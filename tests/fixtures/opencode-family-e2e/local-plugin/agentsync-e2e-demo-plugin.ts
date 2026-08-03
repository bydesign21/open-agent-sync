// Deterministic OW-011 gate fixture: a trivial local OpenCode/Kilo-family
// plugin with no callbacks of its own. Its only job is to prove that a
// `plugins.*.targets.<host>.local` manifest entry is copied into the host's
// real plugin scan directory and actually loads under a live, non-pure host
// process (both OpenCode and Kilo treat any exported async function as a
// plugin) — a concern entirely separate from the generated hooks bridge.
export async function AgentsyncE2eDemoPlugin(_ctx: unknown) {
  return {};
}
