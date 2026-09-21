These three redis-ping runs were meant to be "with VM affinity" but are NOT.
Docker Desktop runs on the WSL2 backend (process vmmemWSL). The affinity was set
on vmmemWSL pid 23296, but that VM restarted (new pid 25884) before these runs,
so the Docker VM was unpinned. meta.json records system.vm_affinity.in_effect =
false. They are effectively additional "without affinity" samples.
