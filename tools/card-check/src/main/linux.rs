use guesthouse_backend::PciAddress;
use guesthouse_backend_nvidia::profile::{HostBranch, host_branch, host_branch_of_version};
use guesthouse_backend_nvidia::rm::device::Device;
use guesthouse_backend_nvidia::rm::linux::LinuxRm;
use guesthouse_card_check::channels::{self, ChannelResult};
use guesthouse_card_check::host::{self, Host, ListedCard, NoHost, no_host_text};
use guesthouse_card_check::report::{self, Branch};
use guesthouse_card_check::survey;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::{env, fs};

const REPORT_NAME: &str = "guesthouse-card-check.txt";
const USAGE: &str = "usage: guesthouse-card-check [--card N | --pci ADDRESS] [--read-only]\n\
                     \n\
                     Reads an NVIDIA card and writes guesthouse-card-check.txt. With more than one card,\n\
                     --card N picks one by its number in the list the program prints, --pci picks one by\n\
                     its PCI address, like 0000:01:00.0, and with neither the program asks.\n\
                     --read-only stops before the channel test.";

const CHANNEL_WARNING: &str = "\
The next step makes a graphics channel and a copy channel on this card, and one on each video engine
where the app runs video. Each writes one value to memory, and then it's freed. This is what a VM does
first. On a card nobody has run this on, the GPU could stop answering, which freezes the desktop on that
card until a reboot. Save your work first.";

const NO_DEVICE_FILES: &str = "\
The driver is loaded but its device files are not there. They are made when something first uses the
card, so running nvidia-smi once usually makes them. Inside a container they are not there at all.";

/// A card asked for on the command line.
enum Wanted {
    Number(usize),
    Address(PciAddress),
}

struct Options {
    card: Option<Wanted>,
    read_only: bool,
}

fn parse_options() -> Result<Options, String> {
    let mut options = Options { card: None, read_only: false };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--card" => {
                let number = arguments.next().ok_or("--card needs a number")?;
                let number = number.parse().map_err(|_| format!("{number} is not a card number"))?;
                options.card = Some(Wanted::Number(number));
            }
            "--pci" => {
                let text = arguments.next().ok_or("--pci needs an address, like 0000:01:00.0")?;
                let address =
                    PciAddress::parse(&text).ok_or_else(|| format!("{text} is not a PCI address"))?;
                options.card = Some(Wanted::Address(address));
            }
            "--read-only" => options.read_only = true,
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(options)
}

/// One line typed in answer, or nothing when there's nobody to type one.
#[allow(clippy::disallowed_methods)] // a line the person types at the terminal
fn answer() -> String {
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    line.trim().to_lowercase()
}

fn choose_card<'a>(host: &'a Host, wanted: Option<&Wanted>) -> Result<(usize, &'a ListedCard), String> {
    let numbered = |number: usize| host.card_numbered(number).ok_or(format!("There is no card {number}."));
    match wanted {
        Some(Wanted::Number(number)) => return numbered(*number),
        Some(Wanted::Address(address)) => {
            return host.card_at(address).ok_or(format!("The driver has no card at {address}."));
        }
        None => {}
    }
    if host.cards.len() == 1 {
        return numbered(0);
    }
    print!("Which card? Type its number: ");
    let _ = io::stdout().flush();
    let typed = answer();
    let number: usize = typed.parse().map_err(|_| format!("{typed:?} is not one of the numbers above."))?;
    numbered(number)
}

/// Writes the report where it was run from, or in the temp folder, and waits for the disk to have
/// it, so that it survives a reset. Says where it went.
fn write_report(text: &str) -> io::Result<PathBuf> {
    let here = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut last = io::Error::other("nowhere to write");
    for folder in [here, env::temp_dir()] {
        let path = folder.join(REPORT_NAME);
        let written = fs::File::create(&path).and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        });
        match written {
            Ok(()) => return Ok(path),
            Err(error) => last = error,
        }
    }
    Err(last)
}

fn save(text: &str) {
    match write_report(text) {
        Ok(path) => println!("\nWritten to {}", path.display()),
        Err(_) => println!("\nCould not write {REPORT_NAME} anywhere. The text above is the report."),
    }
}

fn branch_of(host: &Host) -> (Option<&'static HostBranch>, Branch) {
    let profile = host_branch(host_branch_of_version(&host.version));
    let branch = match profile {
        Some(profile) => {
            Branch::Known { version_tested: profile.tested_versions.contains(&host.version.as_str()) }
        }
        None => Branch::Unknown,
    };
    (profile, branch)
}

fn run(options: &Options) -> ExitCode {
    let host = match host::read_host() {
        Ok(host) => host,
        Err(why) => {
            println!("{}", no_host_text(&why));
            if why == NoHost::NoDriver {
                println!("Guesthouse needs NVIDIA's open kernel modules loaded, with a card on them.");
            }
            return ExitCode::FAILURE;
        }
    };
    let (profile, branch) = branch_of(&host);
    let mut text = report::heading();
    text.push('\n');
    text.push_str(&report::host_lines(&host, branch));
    print!("{text}");

    if !host.open_module {
        println!(
            "\nThe loaded driver is not the open kernel module. Guesthouse only works with the open modules, \
             which most distributions call nvidia-open, so this program stops here."
        );
        save(&text);
        return ExitCode::FAILURE;
    }
    let Some(profile) = profile else {
        println!(
            "\nThe host driver {} is on branch {}, which this build has no profile for. The app would refuse \
             to start a VM, and this program stops here, as talking to a branch it doesn't know could hand \
             it the wrong sizes.",
            host.version,
            host_branch_of_version(&host.version)
        );
        save(&text);
        return ExitCode::FAILURE;
    };
    let (number, listed) = match choose_card(&host, options.card.as_ref()) {
        Ok(chosen) => chosen,
        Err(why) => {
            println!("{why}");
            return ExitCode::FAILURE;
        }
    };
    if host.cards.len() > 1 {
        let note = format!(
            "\nNote: this PC has more than one card. The program finds card {number} by its PCI address and \
             asks the driver which device that is, which has not been tried on a PC with two cards yet, so \
             a wrong answer below could be the program's rather than the card's.\n"
        );
        print!("{note}");
        text.push_str(&note);
    }

    let rm = match LinuxRm::open_card(&host.version, profile, listed.minor) {
        Ok(rm) => rm,
        Err(error) => {
            println!("\nOpening the driver failed: {error}");
            if error.to_string().contains("No such file") {
                println!("{NO_DEVICE_FILES}");
            }
            save(&text);
            return ExitCode::FAILURE;
        }
    };
    let mut device = match Device::open_at(rm, profile, &listed.address) {
        Ok(device) => device,
        Err(error) => {
            let failed = format!(
                "\nOpening card {number} at {}, device file nvidia{}, failed: {error}\n",
                listed.address, listed.minor
            );
            print!("{failed}");
            text.push_str(&failed);
            save(&text);
            return ExitCode::FAILURE;
        }
    };
    let place = format!("\n{}", report::place_line(number, listed, device.instance));
    print!("{place}");
    text.push_str(&place);
    let findings = match survey::survey(&mut device) {
        Ok(findings) => findings,
        Err(error) => {
            println!("\nReading the card failed: {error}");
            save(&text);
            return ExitCode::FAILURE;
        }
    };
    let card_text = format!("\n{}", report::card_lines(number, &findings));
    print!("{card_text}");
    text.push_str(&card_text);
    save(&text);

    if options.read_only {
        return ExitCode::SUCCESS;
    }
    if let Some(why) = channels::refusal(listed, &findings) {
        let why = format!("\n{why}\n");
        print!("{why}");
        text.push_str(&why);
        save(&text);
        return ExitCode::FAILURE;
    }
    println!("\n{CHANNEL_WARNING}");
    print!("Run the channel test? Type y or n: ");
    let _ = io::stdout().flush();
    if answer() != "y" {
        println!("Not run. The file holds the read-only part.");
        return ExitCode::SUCCESS;
    }
    let mut progress = |label: &str| {
        println!("Trying {label}.");
        let trying = format!("{text}\n{}", report::progress_lines(label));
        write_report(&trying).map(drop).map_err(|error| error.to_string())
    };
    let results =
        channels::run_on_listed_card(&mut device, listed, &findings, profile, channels::WAIT, &mut progress);
    let results = match results {
        Ok(results) => results,
        Err(error) => {
            let failed = format!("\nChannel test: not finished: {error}\n");
            print!("{failed}");
            text.push_str(&failed);
            save(&text);
            return ExitCode::FAILURE;
        }
    };
    let channel_text = format!("\n{}", report::channel_lines(&results));
    print!("{channel_text}");
    text.push_str(&channel_text);
    save(&text);
    let passed = results.iter().take(2).all(ChannelResult::released);
    if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

pub fn main() -> ExitCode {
    match parse_options() {
        Ok(options) => run(&options),
        Err(text) => {
            println!("{text}");
            ExitCode::FAILURE
        }
    }
}
