//! `/status` free slots by drone, by the rule idle capacity sums.

use super::*;

#[test]
fn admitting_by_drone_counts_live_memory_and_the_node_pool() {
    // 2026-11-27: pve50-vm240 has a 2360 MiB budget and about 1.5 GiB really
    // free. `/status` counted it a free light slot; no light job (2 GiB) fits.
    let mut small = host("vm240");
    small.vcpu = 1;
    small.mem_mib = 2360;
    small.live.mem_used_mib = Some(900);
    let clear = host("clear");
    let pooled = [on_node("a", 20 * 1024), on_node("b", 20 * 1024)];
    let mut drones = vec![small, clear];
    drones.extend(pooled);
    let by_drone = admitting_by_drone(&drones, &[], &queen_classes(), None);
    let light = |id: &str| by_drone[id][&Class::Light];
    assert_eq!(light("vm240"), 0);
    assert_eq!(light("clear"), 14, "28 GiB of 2 GiB jobs");
    let heavy = |id: &str| by_drone[id][&Class::Heavy];
    // Each pooled drone alone fits two heavy, their node's 16 GiB one.
    assert_eq!(heavy("a") + heavy("b"), 1);
    let idle = idle_after(&drones, &[], &queen_classes(), None);
    let light_idle = idle.iter().find(|c| c.class == Class::Light).unwrap();
    let summed: u32 = by_drone.values().map(|m| m[&Class::Light]).sum();
    assert_eq!(
        light_idle.idle_slots, summed,
        "status and burst read one rule"
    );
}
