# deploy/ — v2 multi-VM 배포 스캐폴드

본 디렉터리는 **단일 물리 호스트 + 하이퍼바이저 + 복수 guest VM** 토폴로지를
구동하기 위한 샘플 스크립트·유닛 파일 모음이다. 실제 운영 시엔 경로, CPU pin,
ivshmem 디바이스 파라미터, systemd 유닛의 `User=`/`Environment=` 등을 **사이트에
맞게 편집**해야 한다.

설계 문서는 `docs/architecture/MULTI_VM_TOPOLOGY.md` 와 `docs/adr/ADR-0003-shm-multi-vm.md`.

## 디렉터리 구성

```
deploy/
├── README.md                    ← 이 문서
├── scripts/
│   ├── host_bootstrap.sh        ← 호스트 사전 점검: grub cmdline / hugetlbfs / cpupower
│   ├── launch_infra_vm.sh       ← publisher + gateway 가 올라갈 infra VM (QEMU, ivshmem master)
│   └── launch_strategy_vm.sh    ← 동일 SHM 영역에 attach 하는 strategy VM
├── systemd/
│   ├── hft-publisher.service    ← infra VM 내에서 publisher 기동
│   └── hft-order-gateway.service← infra VM 내에서 order-gateway 기동
└── tmux/
    └── dev_layout.sh            ← 로컬 dev 환경 — 4분할 터미널로 pub/gw/strat 로그 동시 관찰
```

## 실행 순서 (프로덕션)

1. `scripts/host_bootstrap.sh` 실행 → 호스트 커널 파라미터 / hugetlbfs / CPU
   governor 점검. 통과 시 `docs/adr/ADR-0003-shm-multi-vm.md §10` 의 체크리스트가
   충족된 상태.
2. `scripts/launch_infra_vm.sh` 로 QEMU infra VM 을 띄운다 (ivshmem-plain master).
3. infra VM 안에서 `systemctl --user start hft-publisher hft-order-gateway`.
4. 각 strategy host 에서 `scripts/launch_strategy_vm.sh` 로 guest 추가. 부트 후
   strategy 바이너리가 `hft-strategy-shm` 로 attach.

## 실행 순서 (dev)

```sh
./tmux/dev_layout.sh
```

tmux 세션 `hft-dev` 가 네 개의 pane 으로 뜨며 각 pane 이 publisher / gateway /
strategy-0 / strategy-1 의 로그를 따라간다. `/dev/shm` 기반이므로 단일 호스트에서
VM 없이도 전 경로 검증 가능.

## 주의

- 모든 스크립트는 **비파괴적이도록** `set -u`, `set -e` 기본 + `rm` / `kill` 같은
  destructive 동작은 user confirmation 전까지 수행하지 않는다.
- ivshmem PCI BAR 는 `ftruncate` 불가 → `LayoutSpec` 변경 시 호스트 SHM 파일을
  먼저 재생성한 뒤 모든 VM 을 재부팅해야 한다 (`SHM_VERSION` bump 이벤트).
- hugetlbfs 페이지 수는 `LayoutSpec::total_bytes` 를 1GB 로 나눈 값보다 커야 한다.
