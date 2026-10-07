#!/bin/sh
# Make every source file's mtime a function of its CONTENT across builds.
#
# The /build/target cache mount is shared by every build on this machine —
# the main checkout and each git worktree — and cargo judges a source (and a
# build script's `rerun-if-changed` input) fresh by MTIME: a file whose content
# differs from the last build's, but whose mtime is older than that build's
# outputs, is NOT recompiled. Building two trees in turn therefore mixed them:
# one worktree's gRPC codegen in another's test binary (2026-10-07, `no field
# named max_hops`), a release binary without the change it was built for
# (2026-06-10).
#
# The record (kept in the cache mount, next to what it protects) maps each
# path to the content hash and mtime it last had. A file whose content matches
# its record gets that mtime back — cargo reuses what it built from exactly
# these bytes; a changed or new file gets "now", newer than every output built
# so far, so whatever was built from other bytes is rebuilt.
#
# Call it at the start of EVERY RUN that mounts the target cache, in the same
# RUN as cargo: a separate step would replay from the layer cache while the
# record moves on.
#
# usage: content-mtimes RECORD DIR...
set -eu
record=$1
shift
# One second ahead: cargo compares mtimes to the nanosecond, so "now" must be
# strictly later than an output the previous build wrote this same second.
now=$(($(date +%s) + 1))
for dir in "$@"; do
    [ -d "$dir" ] || continue
    find "$dir" \( -path "$dir/target" -o -name .git \) -prune -o -type f -print0
done | xargs -0 -r sha256sum | perl -e '
    my ($record, $now) = @ARGV;
    @ARGV = ();
    my %seen;
    if (open my $in, "<", $record) {
        while (<$in>) {
            chomp;
            my ($path, $hash, $mtime) = split /\t/;
            $seen{$path} = [$hash, $mtime] if defined $mtime;
        }
        close $in;
    }
    my ($kept, $moved) = (0, 0);
    my %next;
    while (my $line = <STDIN>) {
        chomp $line;
        my ($hash, $path) = $line =~ /^([0-9a-f]{64})  (.*)$/ or next;
        my $known = $seen{$path};
        my $mtime = ($known && $known->[0] eq $hash) ? $known->[1] : $now;
        $mtime == $now ? $moved++ : $kept++;
        utime($mtime, $mtime, $path) or die "content-mtimes: utime $path: $!\n";
        $next{$path} = [$hash, $mtime];
    }
    open my $out, ">", "$record.tmp" or die "content-mtimes: $record.tmp: $!\n";
    print $out join("\t", $_, @{ $next{$_} }), "\n" for sort keys %next;
    close $out or die "content-mtimes: $record.tmp: $!\n";
    rename "$record.tmp", $record or die "content-mtimes: $record: $!\n";
    print STDERR "content-mtimes: $kept unchanged, $moved changed or new\n";
' "$record" "$now"
