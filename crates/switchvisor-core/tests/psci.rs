use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use switchvisor_core::{
    IPA_LIMIT,
    payload::RESIDENT_BASE,
    psci::{self, Cpu, Launch},
};
const LAUNCH: Launch = Launch {
    entry: 0x8020_0000,
    context: 0xfedc_ba98_7654_3210,
};

#[test]
fn only_permitted_aarch64_normal_memory_can_be_an_entry() {
    for entry in [
        0,
        0x70019000,
        0x7ffffffc,
        0x80200001,
        RESIDENT_BASE,
        RESIDENT_BASE + 0xfffffc,
        IPA_LIMIT,
        u64::MAX,
    ] {
        let cpu = Cpu::new();
        assert_eq!(
            cpu.request(Launch { entry, ..LAUNCH }, || panic!(
                "invalid entry booted hardware"
            )),
            psci::INVALID_PARAMS
        );
        assert_eq!(cpu.affinity(), i64::from(psci::OFF));
    }
    for entry in [0x80000000, RESIDENT_BASE - 4, 0xffc00000, IPA_LIMIT - 4] {
        assert!(psci::guest_entry(entry));
    }
    for target in [4, 0x100, 0x80000001, 1 << 32, u64::MAX] {
        assert_eq!(psci::cpu_id(target), None);
    }
    assert_eq!(psci::cpu_id(3), Some(3));
}

#[test]
fn power_state_and_full_context_survive_virtual_off_on() {
    let cpu = Cpu::new();
    assert_eq!(cpu.affinity(), 1);
    assert_eq!(cpu.request(LAUNCH, || 0), 0);
    assert_eq!(cpu.affinity(), 2);
    assert_eq!(cpu.request(LAUNCH, || panic!()), psci::ON_PENDING);
    assert_eq!(cpu.take_launch(), Some(LAUNCH));
    assert_eq!(cpu.affinity(), 0);
    assert_eq!(cpu.request(LAUNCH, || panic!()), psci::ALREADY_ON);
    cpu.power_off();
    let next = Launch {
        context: 42,
        ..LAUNCH
    };
    assert_eq!(
        cpu.request(next, || panic!("physical CPU must remain in EL2")),
        0
    );
    assert_eq!(cpu.take_launch(), Some(next));
}

#[test]
fn firmware_rejection_restores_off_and_allows_a_fresh_request() {
    let cpu = Cpu::new();
    assert_eq!(cpu.request(LAUNCH, || -6), -6);
    assert_eq!(cpu.affinity(), 1);
    assert_eq!(cpu.take_launch(), None);
    let next = Launch {
        entry: 0x80400000,
        context: 77,
    };
    assert_eq!(cpu.request(next, || 0), 0);
    assert_eq!(cpu.take_launch(), Some(next));
}

#[test]
fn firmware_acceptance_precedes_launch_publication() {
    let cpu = Arc::new(Cpu::new());
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let target = cpu.clone();
    let worker = std::thread::spawn(move || {
        target.request(LAUNCH, || {
            started_tx.send(()).unwrap();
            finish_rx.recv().unwrap();
            0
        })
    });
    started_rx.recv().unwrap();
    assert_eq!(cpu.affinity(), 2);
    assert_eq!(cpu.take_launch(), None);
    assert_eq!(cpu.request(LAUNCH, || panic!()), psci::ON_PENDING);
    finish_tx.send(()).unwrap();
    assert_eq!(worker.join().unwrap(), 0);
    assert_eq!(cpu.take_launch(), Some(LAUNCH));
}

#[test]
fn competing_cpu_on_requests_boot_hardware_once_and_keep_the_winners_context() {
    let cpu = Arc::new(Cpu::new());
    let barrier = Arc::new(Barrier::new(16));
    let boots = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..16)
        .map(|context| {
            let (cpu, barrier, boots) = (cpu.clone(), barrier.clone(), boots.clone());
            std::thread::spawn(move || {
                barrier.wait();
                (
                    context,
                    cpu.request(Launch { context, ..LAUNCH }, || {
                        boots.fetch_add(1, Ordering::Relaxed);
                        0
                    }),
                )
            })
        })
        .collect();
    let results: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let winners: Vec<_> = results.iter().filter(|(_, result)| *result == 0).collect();
    assert_eq!(winners.len(), 1);
    assert!(
        results
            .iter()
            .all(|(_, r)| *r == 0 || *r == psci::ON_PENDING)
    );
    assert_eq!(boots.load(Ordering::Relaxed), 1);
    assert_eq!(cpu.take_launch().unwrap().context, winners[0].0);
}

#[test]
fn boot_cpu_is_online_without_another_firmware_request() {
    let cpu = Cpu::new();
    cpu.initialize_boot_cpu();
    assert_eq!(cpu.affinity(), 0);
    assert_eq!(cpu.request(LAUNCH, || panic!()), psci::ALREADY_ON);
    cpu.power_off();
    assert_eq!(cpu.request(LAUNCH, || panic!()), 0);
}

#[test]
fn capabilities_cover_virtual_cpu_power_but_reject_suspend() {
    for fid in [
        psci::CPU_ON32,
        psci::CPU_ON64,
        psci::CPU_OFF,
        psci::AFFINITY32,
        psci::AFFINITY64,
    ] {
        assert_eq!(psci::feature(fid), Some(0));
    }
    for fid in [0x84000001, 0xc4000001, 0x8400000e, 0xc400000e] {
        assert_eq!(psci::feature(fid), Some(-1));
    }
    assert_eq!(psci::feature(0x84000000), None);
}
