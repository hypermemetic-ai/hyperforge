#!/usr/bin/env bash
# naming-gate — a Plexus backend is a *service*, never a *hub* (owner,
# 2026-10-29: "we don't call them hubs anymore. we call them services";
# ~/AGENTS.md "Naming"). This gate fails on any remnant of *hub* in the
# repository: the word (`hub`, `hubs`, `HUB`) and the syllable inside an
# identifier (`DynamicHub`, `build_hub`, `hub-codegen`, `HUB_PORT`).
#
#   scripts/naming-gate.sh          check every file git tracks or would track
#   scripts/naming-gate.sh -c       print only the count of offending lines
#
# Exit 0: none. Exit 1: each offending line printed as path:line:text.
#
# What may stay is a foreign name we do not own, and only that:
#   - GitHub / github / GITHUB_* — the forge's own name, everywhere.
#   - the per-file entries in ALLOW below, each with its reason.
# This file itself is not scanned: it has to spell the word to find it.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
self="scripts/naming-gate.sh"
count_only=0
[ "${1:-}" = "-c" ] && count_only=1

# path-regex <TAB> line-regex (Perl, case-sensitive) <TAB> why.
# A line matching both has the line-regex's match taken out before the
# check, so anything else on that line is still caught.
ALLOW=$(cat <<'ALLOWED'
src/registry\.rs$	"registry-hub"	foreign: plexus-registry's root namespace on the wire today (its --namespace default), tried after "registry-service"; goes when plexus-registry is renamed (no NAMING leaf covers it yet)
\.rs$	\bDynamicHub as DynamicService\b	the plexus-core 0.6.1 type under its published name, imported under the new one; removed by NAMING-17 (after HYPERFORGE-14)
ALLOWED
)

cd "$root"
git ls-files -z --cached --others --exclude-standard \
  | ALLOW="$ALLOW" SELF="$self" COUNT_ONLY="$count_only" perl -0 -ne '
    BEGIN {
      @allow = map { [split /\t/] } grep { /\S/ && !/^\s*#/ } split /\n/, $ENV{ALLOW};
      $n = 0;
    }
    chomp; my $f = $_;
    next if $f eq $ENV{SELF} || ! -f $f || -l $f;
    open my $fh, "<", $f or next;
    binmode $fh;
    my $head; read $fh, $head, 8000; next if defined $head && $head =~ /\0/;   # binary
    seek $fh, 0, 0;
    local $/ = "\n";
    while (my $l = <$fh>) {
      next unless $l =~ /hub/i;
      my $t = $l;
      $t =~ s/github//gi;                       # the forge: GitHub, github.com, GITHUB_TOKEN
      for my $a (@allow) { my ($p, $r) = @$a; $t =~ s/$r//g if $f =~ /$p/; }
      next unless $t =~ /hub/i;
      $n++;
      unless ($ENV{COUNT_ONLY}) { chomp $l; print "$f:$.:$l\n"; }
    }
    close $fh;
    END {
      if ($ENV{COUNT_ONLY}) { print "$n\n"; }
      elsif ($n) { print STDERR "naming-gate: $n line(s) still say hub — a Plexus backend is a service (~/AGENTS.md \"Naming\")\n"; }
      exit($n ? 1 : 0);
    }'
