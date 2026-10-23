#![allow(clippy::indexing_slicing, reason = "tests index known-length fixtures")]

use super::*;

/// The owner's defaults for a 10 GiB, 2 vCPU, 2 GiB guest.
const GROWTH: Growth = Growth {
    cpus_max: 4,
    memory_mib_max: 4096,
    disk_max_gib: 30,
    disk_step_gib: 10,
    disk_free_percent: 15,
    disk_free_gib: 4,
};

/// A calm guest: plenty of space, idle CPU, little memory in use.
fn calm(tick: u64) -> Sample {
    Sample {
        fs_size: 9 * GIB,
        fs_avail: 7 * GIB,
        cpu: CpuTimes {
            busy: tick * 10,
            total: tick * 100,
        },
        mem_total: 2048 * MIB,
        mem_avail: 1500 * MIB,
        online_cpus: 2,
    }
}

const NOW: Current = Current {
    disk_bytes: 10 * GIB,
    vcpus: 2,
    memory_bytes: 2048 * MIB,
};

/// A planner past its first filesystem check.
fn settled() -> Planner {
    let mut p = Planner::new(GROWTH);
    assert_eq!(p.plan(&calm(0), &NOW), vec![Action::GrowFs]);
    p.fs_grown(true);
    p
}

#[test]
fn first_sample_grows_the_filesystem_to_fill_the_disk() {
    let mut p = Planner::new(GROWTH);
    assert_eq!(p.plan(&calm(0), &NOW), vec![Action::GrowFs]);
    p.fs_grown(true);
    assert!(p.plan(&calm(1), &NOW).is_empty());
}

#[test]
fn low_space_grows_the_disk_by_one_step_then_the_filesystem() {
    let mut p = settled();
    let low = Sample {
        fs_avail: 3 * GIB,
        ..calm(1)
    };
    assert_eq!(
        p.plan(&low, &NOW),
        vec![
            Action::GrowDisk {
                from: 10 * GIB,
                to: 20 * GIB
            },
            Action::GrowFs
        ]
    );
}

#[test]
fn the_free_space_floor_is_the_larger_of_percent_and_gib() {
    let mut p = settled();
    let big = Sample {
        fs_size: 100 * GIB,
        fs_avail: 14 * GIB,
        ..calm(1)
    };
    assert!(
        matches!(
            p.plan(&big, &NOW)[..],
            [Action::GrowDisk { .. }, Action::GrowFs]
        ),
        "14 GiB free of 100 GiB is under 15%"
    );
    let mut p = settled();
    let enough = Sample {
        fs_avail: 4 * GIB,
        ..calm(1)
    };
    assert!(
        p.plan(&enough, &NOW).is_empty(),
        "4 GiB free meets the floor"
    );
}

#[test]
fn a_failed_filesystem_grow_is_retried_before_the_disk_grows_again() {
    let mut p = settled();
    let low = Sample {
        fs_avail: GIB,
        ..calm(1)
    };
    assert_eq!(p.plan(&low, &NOW).len(), 2);
    p.fs_grown(false);
    let grown = Current {
        disk_bytes: 20 * GIB,
        ..NOW
    };
    assert_eq!(p.plan(&low, &grown), vec![Action::GrowFs]);
    p.fs_grown(true);
    assert!(matches!(p.plan(&low, &grown)[0], Action::GrowDisk { .. }));
}

#[test]
fn the_disk_stops_at_its_cap_and_says_so_once() {
    let mut p = settled();
    let low = Sample {
        fs_avail: GIB,
        ..calm(1)
    };
    let near = Current {
        disk_bytes: 25 * GIB,
        ..NOW
    };
    assert_eq!(
        p.plan(&low, &near)[0],
        Action::GrowDisk {
            from: 25 * GIB,
            to: 30 * GIB
        }
    );
    p.fs_grown(true);
    let full = Current {
        disk_bytes: 30 * GIB,
        ..NOW
    };
    assert_eq!(
        p.plan(&low, &full),
        vec![Action::DiskAtMax { max: 30 * GIB }]
    );
    assert!(p.plan(&low, &full).is_empty());
}

/// A sample where the CPU was fully busy since `calm(tick - 1)`.
fn busy(tick: u64) -> Sample {
    Sample {
        cpu: CpuTimes {
            busy: tick * 100,
            total: tick * 100,
        },
        ..calm(tick)
    }
}

#[test]
fn sustained_cpu_pressure_doubles_vcpus_up_to_the_cap() {
    let mut p = settled();
    for tick in 1..=SUSTAINED_SAMPLES {
        let actions = p.plan(&busy(u64::from(tick)), &NOW);
        if tick < SUSTAINED_SAMPLES {
            assert!(actions.is_empty(), "no raise after {tick} busy samples");
        } else {
            assert_eq!(actions, vec![Action::RaiseCpus { from: 2, to: 4 }]);
        }
    }
    let at_cap = Current { vcpus: 4, ..NOW };
    for tick in 4..10 {
        assert!(p.plan(&busy(tick), &at_cap).is_empty());
    }
}

#[test]
fn a_calm_sample_resets_cpu_pressure() {
    let mut p = settled();
    let mut ticks = 1..;
    let mut next = || ticks.next().unwrap_or_default();
    for _ in 1..SUSTAINED_SAMPLES {
        assert!(p.plan(&busy(next()), &NOW).is_empty());
    }
    let calm_tick = next();
    let mut gentle = calm(calm_tick);
    gentle.cpu.busy = busy(calm_tick - 1).cpu.busy + 50;
    assert!(p.plan(&gentle, &NOW).is_empty());
    assert!(p.plan(&busy(next()), &NOW).is_empty());
}

#[test]
fn exactly_ninety_percent_cpu_and_eighty_percent_memory_are_not_pressure() {
    let mut p = settled();
    for tick in 1..=5 {
        let s = Sample {
            cpu: CpuTimes {
                busy: tick * 90,
                total: tick * 100,
            },
            mem_total: 2000 * MIB,
            mem_avail: 400 * MIB,
            ..calm(tick)
        };
        assert!(p.plan(&s, &NOW).is_empty());
    }
}

#[test]
fn two_samples_over_eighty_percent_memory_double_it_up_to_the_cap() {
    let mut p = settled();
    let tight = |tick| Sample {
        mem_avail: 400 * MIB,
        ..calm(tick)
    };
    assert!(
        p.plan(&tight(1), &NOW).is_empty(),
        "one sample is not sustained"
    );
    assert_eq!(
        p.plan(&tight(2), &NOW),
        vec![Action::RaiseMemory {
            from: 2048 * MIB,
            to: 4096 * MIB
        }]
    );
    let at_cap = Current {
        memory_bytes: 4096 * MIB,
        ..NOW
    };
    for tick in 4..10 {
        assert!(p.plan(&tight(tick), &at_cap).is_empty());
    }
}

#[test]
fn a_memory_raise_is_whole_hotplug_blocks() {
    let mut p = Planner::new(Growth {
        memory_mib_max: 2048 + 200,
        ..GROWTH
    });
    p.fs_grown(true);
    let tight = |tick| Sample {
        mem_avail: 0,
        ..calm(tick)
    };
    let mut last = Vec::new();
    for tick in 0..u64::from(MEM_SUSTAINED_SAMPLES) {
        last = p.plan(&tight(tick), &NOW);
    }
    assert_eq!(
        last,
        vec![Action::RaiseMemory {
            from: 2048 * MIB,
            to: (2048 + 128) * MIB
        }]
    );
}
