//! Why a machine can't start on this PC now. The judging is plain data in and words out, so a test
//! can state any host it likes.

use crate::context::Context;
use crate::failure::Result;
use crate::gpu_maker::GpuMaker;
use crate::gpu_support::{NO_GPU_PART, gpu_ready};
use crate::held_memory::used_by_others;
use crate::host::{self, GIB, physical_cores, ram_limit_for, vcache_cpus};
use crate::machine::{Kind, Machine};
use crate::network_settings::{network_start_problems, other_machines};
use crate::processes::{memory_taken, qemu_pid};
use crate::settings::max_vram_gib;
use crate::setup::amd::amd_start_problems;
use crate::setup::cards::card_problem;
use crate::setup::intel::NO_INTEL_SUPPORT;
use crate::setup::setup_done;
use crate::tasks::no_slot_text;
use std::path::Path;

pub const SHARE_REFUSAL: &str = "This folder can't be shared. Choose a folder inside your home that isn't hidden, and \
                             isn't one Guesthouse keeps its own files in.";
const ALREADY_RUNNING: &str = "This machine is running.";
pub const MEMORY_TAKEN: &str =
    "Another VM on this PC has just taken this slot's memory. Start the machine again.";
pub const SECOND_INSTANCE_REFUSAL: &str = "A second instance leaves GPU-PV alone.";
/// The daemon can't start a GPU machine without what setup takes from NVIDIA's installer.
pub const NO_SETUP: &str =
    "A GPU machine needs setup first, and it hasn't been run. Run Setup Again from the menu.";
/// Without it the app can't reach the guest's agent, so the display driver and the rest of what a
/// start does in Windows would never happen. Only an install makes one, so a machine from before
/// each had its own must be given the old one.
pub const NO_TOKEN: &str = "This machine has no guest token, so the app can't reach Windows to finish starting it. \
                            Choose the token file it was installed with in its settings, under Guest Token.";

/// What several machines share, read once from the PC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedFacts {
    /// The most RAM this machine can have on this PC now.
    pub ram_limit: u64,
    /// How much of the PC's RAM the other VMs' memory already takes, in GiB.
    pub touched_gib: u64,
    pub max_vram_gib: u32,
    pub vcache_cpus: u32,
}

impl SharedFacts {
    pub fn read(context: &Context, machine: &Machine) -> Result<Self> {
        Ok(Self {
            ram_limit: ram_limit_for(context, machine)?,
            touched_gib: used_by_others(context, context.slot) / GIB,
            max_vram_gib: max_vram_gib(context),
            vcache_cpus: vcache_cpus(&context.cpus).len() as u32,
        })
    }
}

/// What the machines running now leave of the card. One with no cap has all of it.
pub fn video_memory_left(others: &[Machine], max_vram_gib: u32) -> u32 {
    let gpu = others.iter().filter(|other| other.has_gpu());
    let used: u32 = gpu.map(|other| if other.vram_gib > 0 { other.vram_gib } else { max_vram_gib }).sum();
    max_vram_gib.saturating_sub(used)
}

/// What several machines share: the memory that holds them, and the card's. None of it counts if
/// this one runs alone, so a single VM is told what it always was.
pub fn shared_problems(machine: &Machine, others: &[Machine], facts: &SharedFacts) -> Vec<String> {
    if others.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    // The limit already leaves out what they hold, which is all of their RAM once claimed. A machine
    // still starting may not have claimed its own yet, and that will come out of the same RAM.
    let reserved: u64 =
        others.iter().filter(|other| other.has_gpu()).map(|other| u64::from(other.ram_gib)).sum();
    let untouched = reserved.saturating_sub(facts.touched_gib);
    if machine.has_gpu() && u64::from(machine.ram_gib) + untouched > facts.ram_limit {
        found.push("This PC's RAM cannot hold this machine beside the ones running.".to_owned());
    }
    if machine.has_gpu() {
        let left = video_memory_left(others, facts.max_vram_gib);
        if left == 0 {
            found.push(
                "The running machine has all the video memory. Give it a cap in its settings first."
                    .to_owned(),
            );
        } else if machine.vram_gib > left {
            found.push(format!("Only {left} GiB of video memory is left beside the machines running."));
        }
    }
    let pinned: u32 = others.iter().filter(|other| other.vcache_pin).map(|other| other.cores).sum();
    if machine.vcache_pin && pinned + machine.cores > facts.vcache_cpus {
        found.push("The V-Cache cores are taken by the machines running.".to_owned());
    }
    found
}

/// The cap a machine starts with: its own, or what the others leave. Alone it needs none.
pub fn video_cap(machine: &Machine, others: &[Machine], max_vram_gib: u32) -> u32 {
    match (machine.vram_gib, others.is_empty()) {
        (0, false) => video_memory_left(others, max_vram_gib),
        (cap, _) => cap,
    }
}

/// Why this folder can't be shared. A guest can write to what it's given, so it can't be given the
/// user's home as a whole, the hidden folders in it, or what this app runs from.
pub fn share_problem(context: &Context, folder: &str) -> Option<String> {
    let folder = Path::new(folder).canonicalize().ok()?;
    let paths = &context.paths;
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let home = canonical(&paths.home);
    let ours = [&paths.store, &paths.config, &paths.state].map(|path| canonical(path));
    let holds_ours = ours.iter().any(|path| path.starts_with(&folder) || folder.starts_with(path));
    let hidden = folder.strip_prefix(&home).is_ok_and(|inside| {
        inside.components().next().is_some_and(|first| first.as_os_str().to_string_lossy().starts_with('.'))
    });
    (folder == home || home.starts_with(&folder) || holds_ours || hidden).then(|| SHARE_REFUSAL.to_owned())
}

/// Says why a GPU machine can't start on its card, by the maker whose setup and card it needs.
fn card_start_problems(context: &Context, machine: &Machine) -> Vec<String> {
    match machine.windows_maker() {
        Some(GpuMaker::Nvidia) => {
            if setup_done(context) {
                card_problem(context).into_iter().collect()
            } else {
                vec![NO_SETUP.to_owned()]
            }
        }
        Some(GpuMaker::Amd) => amd_start_problems(context, &machine.amd_gpu),
        Some(GpuMaker::Intel) => vec![NO_INTEL_SUPPORT.to_owned()],
        None => Vec::new(),
    }
}

/// Says why this machine can't start on this host now.
pub fn start_problems(context: &Context, machine: &Machine) -> Result<Vec<String>> {
    let mut found = machine.problems(max_vram_gib(context));
    for path in [&machine.disk, &machine.vars] {
        if !path.is_empty() && !Path::new(path).exists() {
            found.push(format!("{path} is missing."));
        }
    }
    if !machine.iso.is_empty() && !Path::new(&machine.iso).exists() {
        found.push(format!("{} is missing. Eject it in the settings to boot from the disk.", machine.iso));
    }
    if !machine.share.is_empty() && !Path::new(&machine.share).is_dir() {
        found.push(format!("{} is missing. Choose another shared folder in the settings.", machine.share));
    }
    found.extend(share_problem(context, &machine.share));
    if machine.kind == Kind::Windows && !gpu_ready(context) {
        found.push(NO_GPU_PART.to_owned());
    } else if machine.has_gpu() {
        found.extend(card_start_problems(context, machine));
        if !machine.token_file(context).exists() {
            found.push(NO_TOKEN.to_owned());
        }
    }
    if machine.has_gpu() && context.second_instance() {
        found.push(SECOND_INSTANCE_REFUSAL.to_owned());
    }
    let cores = physical_cores(&context.cpus);
    if machine.cores > cores {
        found.push(format!("This PC has {cores} cores, so a machine can have that many at most."));
    }
    let limit = ram_limit_for(context, machine)?;
    if u64::from(machine.ram_gib) > limit {
        found.push(format!("This PC can give a machine {limit} GiB at most right now."));
    }
    if machine.vcache_pin {
        let cpus = host::vcache_cpus(&context.cpus);
        if cpus.is_empty() {
            found.push("This CPU has no V-Cache cores to pin to.".to_owned());
        } else if machine.cores as usize > cpus.len() {
            found.push(format!(
                "The V-Cache cores run {} threads, too few for {} cores.",
                cpus.len(),
                machine.cores
            ));
        }
    }
    let others = other_machines(context);
    if others.iter().any(|other| other.disk == machine.disk) {
        found.push(ALREADY_RUNNING.to_owned());
    }
    if qemu_pid(context, None).is_some() {
        found.push(no_slot_text(context));
    } else if machine.has_gpu() && memory_taken(context, None) {
        found.push(MEMORY_TAKEN.to_owned());
    }
    found.extend(shared_problems(machine, &others, &SharedFacts::read(context, machine)?));
    found.extend(network_start_problems(context, machine, &others));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::stand_in::fake_cpus;
    use crate::machine::Kind;
    use crate::processes::stand_in::fake_process;
    use crate::slots;
    use crate::testing::{Scratch, scratch_context};
    use std::fs;

    fn machine(id: &str) -> Machine {
        Machine::new(id, id, &format!("/{id}"), "/v")
    }

    fn facts() -> SharedFacts {
        SharedFacts { ram_limit: 36, touched_gib: 0, max_vram_gib: 28, vcache_cpus: 0 }
    }

    fn says(found: &[String], text: &str) -> bool {
        found.iter().any(|problem| problem.contains(text))
    }

    #[test]
    fn the_home_folder_and_whats_above_it_cant_be_shared() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.paths.home = scratch.path("home");
        fs::create_dir_all(&context.paths.home).unwrap();
        let home = context.paths.home.display().to_string();
        assert!(share_problem(&context, &home).is_some());
        assert!(share_problem(&context, &scratch.path("").display().to_string()).is_some());
    }

    #[test]
    fn hidden_folders_and_the_apps_own_cant_be_shared() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.paths.home = scratch.path("home");
        let hidden = scratch.path("home/.config/autostart");
        fs::create_dir_all(&hidden).unwrap();
        fs::create_dir_all(context.paths.machines.join("one")).unwrap();
        assert!(share_problem(&context, &hidden.display().to_string()).is_some());
        let machine = context.paths.machines.join("one").display().to_string();
        assert!(share_problem(&context, &machine).is_some());
    }

    #[test]
    fn a_folder_of_its_own_can_be_shared() {
        let scratch = Scratch::new();
        let mut context = scratch_context(&scratch);
        context.paths.home = scratch.path("home");
        let documents = scratch.path("home/Documents/For the guest");
        fs::create_dir_all(&documents).unwrap();
        assert_eq!(share_problem(&context, &documents.display().to_string()), None);
    }

    #[test]
    fn one_with_no_cap_has_all_the_card() {
        let (first, second) = (machine("first"), machine("second"));
        let others = [first.clone()];
        assert_eq!(video_memory_left(&others, 28), 0);
        assert!(says(&shared_problems(&second, &others, &facts()), "all the video memory"));
    }

    #[test]
    fn a_cap_leaves_the_rest_for_the_next_machine() {
        let others = [Machine { vram_gib: 10, ..machine("first") }];
        let mut second = machine("second");
        assert_eq!(video_memory_left(&others, 28), 18);
        assert!(shared_problems(&second, &others, &facts()).is_empty());
        assert_eq!(video_cap(&second, &others, 28), 18, "what the first leaves");
        second.vram_gib = 20;
        assert!(says(&shared_problems(&second, &others, &facts()), "18 GiB"));
    }

    #[test]
    fn each_machine_counts_in_full_against_the_pcs_ram() {
        let others = [Machine { vram_gib: 10, ..machine("first") }];
        let small = SharedFacts { ram_limit: 30, ..facts() };
        assert!(says(&shared_problems(&machine("second"), &others, &small), "PC's RAM cannot hold"));
        assert!(shared_problems(&machine("second"), &others, &facts()).is_empty());
        let touched = SharedFacts { touched_gib: 16, ..small };
        assert!(
            shared_problems(&machine("second"), &others, &touched).is_empty(),
            "what is touched is in the limit"
        );
    }

    #[test]
    fn the_vcache_cores_count_together() {
        let others = [Machine { vcache_pin: true, vram_gib: 10, ..machine("first") }];
        let second = Machine { vcache_pin: true, cores: 9, ..machine("second") };
        let with_cores = SharedFacts { vcache_cpus: 16, ..facts() };
        assert!(says(&shared_problems(&second, &others, &with_cores), "V-Cache"));
    }

    #[test]
    fn alone_nothing_is_shared() {
        assert!(shared_problems(&machine("second"), &[], &facts()).is_empty());
        assert_eq!(video_cap(&machine("second"), &[], 28), 0);
    }

    fn startable(scratch: &Scratch) -> (Context, Machine) {
        let mut context = scratch_context(scratch);
        context.cpus = fake_cpus(scratch, "cpus", &[32768; 16]);
        context.meminfo =
            scratch.write("meminfo", "MemTotal:       100000000 kB\nMemAvailable:   100000000 kB\n");
        let disk = scratch.write("disk", "").display().to_string();
        let vars = scratch.write("vars", "").display().to_string();
        let machine = Machine { kind: Kind::Plain, ..Machine::new("vm", "VM", &disk, &vars) };
        (context, machine)
    }

    #[test]
    fn a_ready_plain_machine_has_no_problems() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        assert_eq!(start_problems(&context, &machine).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn missing_things_are_named() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        let gone = scratch.path("gone").display().to_string();
        let problems = |machine: &Machine| start_problems(&context, machine).unwrap();
        assert!(says(&problems(&Machine { disk: gone.clone(), ..machine.clone() }), "is missing"));
        assert!(says(&problems(&Machine { iso: gone.clone(), ..machine.clone() }), "Eject it"));
        assert!(says(&problems(&Machine { share: gone, ..machine.clone() }), "shared folder"));
    }

    #[test]
    fn more_than_the_pc_has_is_refused() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        let found = start_problems(&context, &Machine { cores: 9, ram_gib: 1000, ..machine }).unwrap();
        assert!(says(&found, "8 cores") && says(&found, "GiB at most"));
    }

    #[test]
    fn pinning_needs_vcache_cores() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        let found = start_problems(&context, &Machine { vcache_pin: true, ..machine }).unwrap();
        assert!(says(&found, "no V-Cache"));
    }

    #[test]
    fn a_second_instance_leaves_the_gpu_alone() {
        let scratch = Scratch::new();
        let (mut context, machine) = startable(&scratch);
        context.paths.own_run = Some(scratch.path("second"));
        let gpu = Machine { kind: Kind::Windows, ..machine };
        assert!(says(&start_problems(&context, &gpu).unwrap(), "leaves GPU-PV alone"));
    }

    fn finish_setup(context: &Context) {
        fs::create_dir_all(crate::settings::driver_folder(context)).unwrap();
        let card = serde_json::json!({"card": {"device": 1}});
        crate::settings::write_setup(context, card.as_object().cloned().unwrap()).unwrap();
    }

    #[test]
    fn a_gpu_machine_needs_setup_first() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        let gpu = Machine { kind: Kind::Windows, ..machine };
        let found = start_problems(&context, &gpu).unwrap();
        assert!(says(&found, NO_SETUP), "{found:?}");
        assert!(found[0].contains("Run Setup Again"), "it comes first, so it's what the row says");
        finish_setup(&context);
        assert!(!says(&start_problems(&context, &gpu).unwrap(), NO_SETUP));
    }

    #[test]
    fn a_windows_machine_on_an_amd_card_needs_amds_driver_and_not_nvidias_setup() {
        use crate::setup::amd::NO_AMD_SETUP;
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        let on_amd = Machine { kind: Kind::Windows, amd_gpu: "0000:7c:00.0".into(), ..machine };
        let found = start_problems(&context, &on_amd).unwrap();
        assert!(says(&found, NO_AMD_SETUP), "{found:?}");
        assert!(says(&found, "at 0000:7c:00.0, isn't in this PC now"), "{found:?}");
        assert!(!says(&found, NO_SETUP), "NVIDIA's setup is not this machine's");
    }

    #[test]
    fn a_windows_machine_on_an_intel_card_is_refused_plainly_even_when_the_other_makers_are_set_up() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        finish_setup(&context);
        let mut on_intel = Machine { kind: Kind::Windows, ..machine };
        on_intel.use_windows_card(GpuMaker::Intel, "0000:00:02.0");
        fs::create_dir_all(on_intel.folder(&context)).unwrap();
        crate::agent::make_token(&context, &on_intel).unwrap();
        let found = start_problems(&context, &on_intel).unwrap();
        assert_eq!(found, [NO_INTEL_SUPPORT], "NVIDIA's setup being done changes nothing");
    }

    #[test]
    fn on_two_cards_a_gpu_machine_needs_setup_to_have_named_one() {
        use crate::setup::cards::stand_in::{NAMES, fake_pci};
        let scratch = Scratch::new();
        let (mut context, machine) = startable(&scratch);
        context.pci_devices = scratch.path("pci");
        context.pci_names = scratch.write("pci.ids", NAMES);
        fake_pci(&scratch, "0000:01:00.0", 0x2704, "0x10de", "0x030000");
        let gpu = Machine { kind: Kind::Windows, ..machine };
        finish_setup(&context);
        assert!(!says(&start_problems(&context, &gpu).unwrap(), "Run Setup Again"), "one card, an old setup");
        fake_pci(&scratch, "0000:02:00.0", 0x21C4, "0x10de", "0x030000");
        assert!(says(&start_problems(&context, &gpu).unwrap(), "more than one NVIDIA card"));
        let card = serde_json::json!({"card": {"device": 0x21C4, "pci": "0000:02:00.0"}});
        crate::settings::write_setup(&context, card.as_object().cloned().unwrap()).unwrap();
        assert!(!says(&start_problems(&context, &gpu).unwrap(), "Run Setup Again"));
    }

    #[test]
    fn only_a_gpu_machine_needs_setup() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        assert!(!says(&start_problems(&context, &machine).unwrap(), NO_SETUP), "a plain VM");
        let installing = Machine { kind: Kind::Windows, installing: true, ..machine };
        assert!(!says(&start_problems(&context, &installing).unwrap(), NO_SETUP), "an install");
    }

    #[test]
    fn a_gpu_machine_with_no_token_says_where_to_give_it_one() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        finish_setup(&context);
        let gpu = Machine { kind: Kind::Windows, ..machine };
        fs::create_dir_all(gpu.folder(&context)).unwrap();
        assert_eq!(start_problems(&context, &gpu).unwrap(), [NO_TOKEN]);
        assert!(NO_TOKEN.contains("Guest Token"), "the settings group the Token row is in");
        crate::agent::make_token(&context, &gpu).unwrap();
        assert!(start_problems(&context, &gpu).unwrap().is_empty());
    }

    #[test]
    fn only_a_gpu_machine_needs_a_token_to_start() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        finish_setup(&context);
        assert!(!says(&start_problems(&context, &machine).unwrap(), NO_TOKEN), "a plain VM");
        let installing = Machine { kind: Kind::Windows, installing: true, ..machine };
        assert!(!says(&start_problems(&context, &installing).unwrap(), NO_TOKEN), "an install makes it");
    }

    #[test]
    fn memory_another_vm_took_says_so_only_for_a_gpu_machine() {
        let scratch = Scratch::new();
        let (context, machine) = startable(&scratch);
        let memory = context.paths.shared_file("ram", slots::first());
        fs::write(&memory, "").unwrap();
        fake_process(
            &context,
            200,
            &["/other/guesthouse-daemon", "/other"],
            &[],
            &memory.display().to_string(),
        );
        let gpu = Machine { kind: Kind::Windows, ..machine.clone() };
        // The fake meminfo says nothing about the fake shared memory's real size, so only the words count.
        assert!(says(&start_problems(&context, &gpu).unwrap(), MEMORY_TAKEN));
        assert!(
            !says(&start_problems(&context, &machine).unwrap(), MEMORY_TAKEN),
            "a plain VM does not use it"
        );
    }
}
