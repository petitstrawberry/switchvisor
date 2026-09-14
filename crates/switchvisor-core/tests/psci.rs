use std::sync::{
    Barrier,
    atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
    mpsc,
};
use std::time::Duration;
use switchvisor_core::{
    IPA_LIMIT,
    payload::RESIDENT_BASE,
    psci::{self, Launch, Machine},
};
const LAUNCH: Launch = Launch {
    entry: 0x8020_0000,
    context: 0xfedc_ba98_7654_3210,
};

#[test]
fn only_permitted_aarch64_normal_memory_can_be_an_entry() {
    let mut machine = Machine::new();
    let [mut boot, ..] = machine.split();
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
        assert_eq!(
            boot.request(1, Launch { entry, ..LAUNCH }, || panic!(
                "invalid entry booted hardware"
            )),
            psci::INVALID_PARAMS
        );
        assert_eq!(boot.affinity(1), i64::from(psci::OFF));
    }
    for entry in [0x80000000, RESIDENT_BASE - 4, 0xffc00000, IPA_LIMIT - 4] {
        assert!(psci::guest_entry(entry));
    }
    for target in [4, 0x100, 0x80000001, 1 << 32, u64::MAX] {
        assert_eq!(psci::cpu_id(target), None);
    }
    assert_eq!(psci::cpu_id(3), Some(3));
    assert_eq!(boot.request(4, LAUNCH, || panic!()), psci::INVALID_PARAMS);
    assert_eq!(boot.affinity(4), psci::INVALID_PARAMS);
}

#[test]
fn power_state_and_full_context_survive_virtual_off_on() {
    let mut machine = Machine::new();
    let [mut boot, mut cpu, ..] = machine.split();
    assert_eq!(boot.affinity(1), 1);
    assert_eq!(boot.request(1, LAUNCH, || 0), 0);
    assert_eq!(boot.affinity(1), 2);
    assert_eq!(boot.request(1, LAUNCH, || panic!()), psci::ON_PENDING);
    assert_eq!(cpu.take_launch(), Some(LAUNCH));
    assert_eq!(boot.affinity(1), 0);
    assert_eq!(boot.request(1, LAUNCH, || panic!()), psci::ALREADY_ON);
    cpu.power_off();
    let next = Launch {
        context: 42,
        ..LAUNCH
    };
    assert_eq!(
        boot.request(1, next, || panic!("physical CPU must remain in EL2")),
        0
    );
    assert_eq!(cpu.take_launch(), Some(next));
}

#[test]
fn firmware_rejection_restores_off_and_allows_a_fresh_request() {
    let mut machine = Machine::new();
    let [mut boot, mut cpu, ..] = machine.split();
    assert_eq!(boot.request(1, LAUNCH, || -6), -6);
    assert_eq!(boot.affinity(1), 1);
    assert_eq!(cpu.take_launch(), None);
    let next = Launch {
        entry: 0x80400000,
        context: 77,
    };
    assert_eq!(boot.request(1, next, || 0), 0);
    assert_eq!(cpu.take_launch(), Some(next));
}

#[test]
fn firmware_acceptance_precedes_launch_publication_without_holding_the_claim_lock() {
    let mut machine = Machine::new();
    let [mut boot, mut cpu, mut other, ..] = machine.split();
    let (started_tx, started_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let (status_tx, status_rx) = mpsc::channel();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            boot.request(1, LAUNCH, || {
                started_tx.send(()).unwrap();
                finish_rx.recv().unwrap();
                0
            })
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let pending = cpu.affinity(1);
        let early_launch = cpu.take_launch();
        let contender = scope.spawn(move || {
            let same_cpu = other.request(1, LAUNCH, || panic!());
            let another_cpu = other.request(3, LAUNCH, || 0);
            status_tx.send((same_cpu, another_cpu)).unwrap();
        });
        let early_status = status_rx.recv_timeout(Duration::from_secs(2));
        // Always let firmware finish, including when the contender timed out.
        finish_tx.send(()).unwrap();
        assert_eq!(worker.join().unwrap(), 0);
        contender.join().unwrap();
        assert_eq!(pending, 2);
        assert_eq!(early_launch, None);
        assert_eq!(early_status.unwrap(), (psci::ON_PENDING, 0));
        assert_eq!(cpu.take_launch(), Some(LAUNCH));
    });
}

#[test]
fn repeated_competing_cpu_on_requests_boot_once_and_keep_each_winners_context() {
    let mut machine = Machine::new();
    let participants = machine.split();
    let barrier = Barrier::new(4);
    let boots = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let statuses = [const { AtomicI64::new(-99) }; 3];
    std::thread::scope(|scope| {
        for (caller, mut cpu) in participants.into_iter().enumerate() {
            let (barrier, boots, failed, statuses) = (&barrier, &boots, &failed, &statuses);
            scope.spawn(move || {
                for round in 0..512 {
                    if caller == 3 {
                        cpu.power_off();
                    }
                    barrier.wait();
                    if caller < 3 {
                        let launch = Launch {
                            entry: LAUNCH.entry + caller as u64 * 4,
                            context: 0xfedc_ba98_0000_0000 | (round << 16) | caller as u64,
                        };
                        let status = cpu.request(3, launch, || {
                            boots.fetch_add(1, Ordering::SeqCst);
                            0
                        });
                        statuses[caller].store(status, Ordering::SeqCst);
                    }
                    barrier.wait();
                    if caller == 3 {
                        let results = statuses.each_ref().map(|s| s.load(Ordering::SeqCst));
                        let winners: Vec<_> = results
                            .iter()
                            .enumerate()
                            .filter(|(_, result)| **result == 0)
                            .map(|(caller, _)| caller)
                            .collect();
                        let launch = cpu.take_launch();
                        let valid = winners.len() == 1
                            && results.iter().all(|r| *r == 0 || *r == psci::ON_PENDING)
                            && launch
                                == Some(Launch {
                                    entry: LAUNCH.entry + winners[0] as u64 * 4,
                                    context: 0xfedc_ba98_0000_0000
                                        | (round << 16)
                                        | winners[0] as u64,
                                });
                        if !valid {
                            failed.store(true, Ordering::SeqCst);
                        }
                    }
                    barrier.wait();
                }
            });
        }
    });
    assert!(!failed.load(Ordering::SeqCst));
    assert_eq!(boots.load(Ordering::SeqCst), 1);
}

#[test]
fn a_launch_can_only_be_consumed_by_its_pinned_cpu() {
    let mut machine = Machine::new();
    let [mut boot, mut cpu, mut other, ..] = machine.split();
    assert_eq!(boot.request(1, LAUNCH, || 0), 0);
    assert_eq!(boot.take_launch(), None);
    assert_eq!(other.take_launch(), None);
    assert_eq!(cpu.take_launch(), Some(LAUNCH));
    assert_eq!(cpu.take_launch(), None);
}

#[test]
fn boot_cpu_is_online_without_another_firmware_request() {
    let mut machine = Machine::new();
    let [mut boot, mut other, ..] = machine.split();
    assert_eq!(boot.affinity(0), 0);
    assert_eq!(boot.request(0, LAUNCH, || panic!()), psci::ALREADY_ON);
    boot.power_off();
    assert_eq!(other.request(0, LAUNCH, || panic!()), 0);
    assert_eq!(boot.take_launch(), Some(LAUNCH));
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
