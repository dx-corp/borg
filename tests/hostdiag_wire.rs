//! Host telemetry stays readable through the public wire types.

#[test]
fn private_host_diagnostic_fields_preserve_the_wire_contract() {
    let wire = serde_json::json!({
        "start": {
            "at": 100,
            "cpus": 16,
            "load": [1.0, 2.0, 3.0],
            "cpu_psi": {
                "some_avg10": 1.0,
                "some_avg60": 2.0,
                "full_avg10": 3.0,
                "full_avg60": 4.0
            },
            "memory_psi": null,
            "io_psi": null,
            "cgroup_cpu_psi": null,
            "cgroup_memory_psi": null,
            "host_cpu_usage_usec": 1000,
            "host_cpu_busy_cores": 2.0,
            "drone_cpu_usage_usec": 500,
            "drone_cpu_throttled_usec": 1,
            "mem_available_mib": 4096,
            "work_free_mib": 20000,
            "cache_free_mib": 30000,
            "active_leases": 2
        },
        "end": null
    });
    let report: borg::hostdiag::LeaseHostReport = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(report).unwrap(), wire);
}
