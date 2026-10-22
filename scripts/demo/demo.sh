#!/bin/sh
# Scripted session recorded into the README GIF. Run by record.sh inside
# a scratch git repo ($DEMO_REPO) with a private HOME and daemon.
cd "$DEMO_REPO" || exit 1
cmd() { printf '$ '; sleep 0.5; printf '%s\n' "$*"; sleep 0.3; }
# Same output as `sluice status`, minus the socket path and job ids so
# every line fits in 80 columns.
st() {
  sluice status | grep -v '^daemon' |
    sed 's/ *job [0-9a-f-]*$//; s/, 0 failed, 0 timed out//; s/ saved$/ saved/'
}
pause() { sleep "$1"; }

cmd cat .sluice.toml
cat .sluice.toml
pause 1.5

cmd sluice run test
sluice run test
pause 1

cmd sluice run test
sluice run test
pause 1.5

printf '# five more worktrees, same tree, six requests at once\n'
pause 1
for i in 1 2 3 4 5; do git worktree add -q -d "../wt$i" HEAD; done
dirs=". ../wt1 ../wt2 ../wt3 ../wt4 ../wt5"
printf '$ '; sleep 0.4
printf '%s\n' 'for d in $DIRS; do (cd $d; echo x > f; sluice run test) & done'
for d in $dirs; do
  ( cd "$d" && echo x > f && sluice run test 2>&1 | sed "s/ (job .*//; s/^sluice: /[$(basename "$(pwd)")] /; s/, sharing job .*//" ) &
  sleep 0.3
done
sleep 0.6
cmd sluice status
st
wait
pause 1
cmd sluice status
st
pause 1
