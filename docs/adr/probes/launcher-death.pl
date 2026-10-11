use strict;
use warnings;
use POSIX ();

# Manual macOS containment probe. Run only inside a disposable, task-owned VM.
# The child and parent both expire after 240 seconds if cleanup fails.
my $directory = shift @ARGV or die "missing output directory\n";
my $child = fork();
defined $child or die "fork: $!\n";

if ($child == 0) {
    POSIX::setsid() >= 0 or die "setsid: $!\n";
    delete $ENV{REEF_RUN_OWNER};
    open my $metadata, '>', "$directory/child-meta" or die "$!\n";
    print {$metadata} "pid=$$ ppid=", getppid(), " marker=cleared\n";
    close $metadata or die "$!\n";
    my $deadline = time() + 240;
    while (time() < $deadline) {
        open my $heartbeat, '>>', "$directory/child-heartbeat" or die "$!\n";
        print {$heartbeat} time(), "\n";
        close $heartbeat or die "$!\n";
        sleep 1;
    }
    exit 0;
}

open my $parent_metadata, '>', "$directory/parent-meta" or die "$!\n";
print {$parent_metadata} "pid=$$ child=$child\n";
close $parent_metadata or die "$!\n";
sleep 240;
