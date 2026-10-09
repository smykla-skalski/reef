use sysinfo::System;

const BYTES_PER_GIB: u64 = 1_073_741_824;

pub fn print() {
    let mut system = System::new_all();
    system.refresh_all();

    println!(
        "CPU: {:.1}% across {} logical cores",
        system.global_cpu_usage(),
        system.cpus().len()
    );
    println!(
        "Memory: {} / {} ({}%)",
        gib(system.used_memory()),
        gib(system.total_memory()),
        percentage(system.used_memory(), system.total_memory())
    );
    println!(
        "Swap: {} / {} ({}%)",
        gib(system.used_swap()),
        gib(system.total_swap()),
        percentage(system.used_swap(), system.total_swap())
    );
}

fn gib(bytes: u64) -> String {
    let tenths = u128::from(bytes) * 10 / u128::from(BYTES_PER_GIB);
    format!("{}.{:01} GiB", tenths / 10, tenths % 10)
}

fn percentage(used: u64, total: u64) -> String {
    if total == 0 {
        return "0.0".to_owned();
    }

    let tenths = u128::from(used) * 1_000 / u128::from(total);
    format!("{}.{:01}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::{gib, percentage};

    #[test]
    fn formats_bytes_as_gibibytes() {
        assert_eq!(gib(1_610_612_736), "1.5 GiB");
    }

    #[test]
    fn calculates_percentage() {
        assert_eq!(percentage(9, 12), "75.0");
    }

    #[test]
    fn treats_empty_capacity_as_zero_percent() {
        assert_eq!(percentage(0, 0), "0.0");
    }
}
