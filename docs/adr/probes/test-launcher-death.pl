use strict;
use warnings;
use Cwd qw(abs_path);
use File::Temp qw(tempdir);
use POSIX qw(WNOHANG);
use Time::HiRes qw(CLOCK_MONOTONIC clock_gettime sleep);

# Destructive by design: run only in a fresh, disposable, task-owned macOS VM.
die "set REEF75_DISPOSABLE_VM=1 only inside a disposable guest\n"
    unless ($ENV{REEF75_DISPOSABLE_VM} // '') eq '1';

my ($reef, $probe, $evidence_root) = @ARGV;
die "usage: $0 <reef-binary> <launcher-death.pl> <evidence-root>\n"
    unless defined $reef && defined $probe && defined $evidence_root && @ARGV == 3;
$reef = abs_path($reef) or die "reef binary not found\n";
$probe = abs_path($probe) or die "probe not found\n";
$evidence_root = abs_path($evidence_root) or die "evidence root not found\n";
die "reef is not executable\n" unless -x $reef;
die "probe is not a file\n" unless -f $probe;
die "evidence root is not writable\n" unless -d $evidence_root && -w $evidence_root;

my $directory = tempdir('launcher-XXXXXX', DIR => $evidence_root, CLEANUP => 0);
my ($launcher, $parent, $child);

sub process_field {
    my ($pid, $field) = @_;
    return '' unless defined $pid && $pid =~ /^[1-9][0-9]*$/;
    open my $ps, '-|', '/bin/ps', '-ww', '-p', $pid, '-o', "$field=" or die "ps: $!\n";
    local $/;
    my $value = <$ps> // '';
    close $ps;
    $value =~ s/^\s+|\s+$//g;
    return $value;
}

sub owned_probe {
    my ($pid) = @_;
    my $command = process_field($pid, 'command');
    return length($command) && index($command, $probe) >= 0 && index($command, $directory) >= 0;
}

sub heartbeat_count {
    my $file = "$directory/child-heartbeat";
    return 0 unless -f $file;
    open my $heartbeat, '<', $file or die "read heartbeat: $!\n";
    my $count = 0;
    $count++ while <$heartbeat>;
    close $heartbeat or die "close heartbeat: $!\n";
    return $count;
}

sub wait_until {
    my ($description, $seconds, $predicate) = @_;
    my $deadline = clock_gettime(CLOCK_MONOTONIC) + $seconds;
    while (clock_gettime(CLOCK_MONOTONIC) < $deadline) {
        return if $predicate->();
        sleep 0.1;
    }
    die "timed out waiting for $description\n";
}

sub cleanup {
    if (defined $launcher && waitpid($launcher, WNOHANG) == 0) {
        kill 'KILL', $launcher;
        waitpid($launcher, 0);
    }
    $launcher = undef;
    for my $pid ($parent, $child) {
        kill 'TERM', $pid if owned_probe($pid);
    }
    sleep 0.3;
    for my $pid ($parent, $child) {
        kill 'KILL', $pid if owned_probe($pid);
    }
    my $clean = !owned_probe($parent) && !owned_probe($child);
    ($parent, $child) = (undef, undef) if $clean;
    return $clean;
}

END {
    if (defined $launcher || defined $parent || defined $child) {
        warn "task process cleanup incomplete\n" unless cleanup();
    }
}

$launcher = fork();
defined $launcher or die "fork: $!\n";
if ($launcher == 0) {
    $launcher = undef;
    open STDOUT, '>', "$directory/reef-stdout" or die "stdout: $!\n";
    open STDERR, '>', "$directory/reef-stderr" or die "stderr: $!\n";
    $ENV{REEF_RUN_OWNER} = 'launcher-repro';
    exec {$reef} $reef, 'run', '--no-record', '--', '/usr/bin/perl', $probe, $directory;
    die "exec reef: $!\n";
}

wait_until('probe metadata', 15, sub { -s "$directory/parent-meta" && -s "$directory/child-meta" });
open my $parent_meta, '<', "$directory/parent-meta" or die "parent metadata: $!\n";
my $parent_line = <$parent_meta> // '';
close $parent_meta;
open my $child_meta, '<', "$directory/child-meta" or die "child metadata: $!\n";
my $child_line = <$child_meta> // '';
close $child_meta;
($parent, my $parent_child) = $parent_line =~ /^pid=(\d+) child=(\d+)\n$/;
($child, my $child_ppid, my $marker) = $child_line =~ /^pid=(\d+) ppid=(\d+) marker=(\w+)\n$/;
die "invalid or inconsistent probe metadata\n"
    unless defined $parent && defined $child && $parent_child == $child && $marker eq 'cleared';
die "probe processes are not task-owned\n" unless owned_probe($parent) && owned_probe($child);
my $child_pgid = process_field($child, 'pgid');
die "child did not create a distinct session\n" unless $child_pgid =~ /^\d+$/ && $child_pgid == $child;
wait_until('two heartbeat lines', 10, sub { heartbeat_count() >= 2 });
my $before = heartbeat_count();

kill 'KILL', $launcher or die "kill Reef launcher: $!\n";
waitpid($launcher, 0) == $launcher or die "reap Reef launcher failed\n";
$launcher = undef;
wait_until('heartbeat after launcher death', 5, sub { heartbeat_count() > $before });
my $after = heartbeat_count();
die "task process cleanup incomplete\n" unless cleanup();
print "CURRENT REEF ESCAPE: child heartbeat advanced $before -> $after after launcher SIGKILL; "
    . "task processes stopped; evidence=$directory\n";
