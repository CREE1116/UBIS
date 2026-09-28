# 측정 기록

2026-09-28, 코어 v0. 모든 수치는 `ubis-bench`로 재현 가능하다. 단일 실행, x86_64 2코어 클라우드 컨테이너.

## 설정

- 시간 분할: 최근 `--holdout` 개 커밋을 질의로 쓰고, 그 직전 커밋($T_0$)의 트리만 색인한다.
- 정답: 커밋의 hunk(변경 후 줄 범위)와 겹치는 leaf unit 중 $T_0$에 존재하는 것. 서수 unit(`¶n`, `~n`)은 같은 파일에서 토큰 Jaccard ≥ 0.3인 가장 비슷한 $T_0$ leaf로 매핑한다.
- `recall`: 정답 unit 중 결과(또는 결과의 자손)로 덮인 비율의 평균. `hit`: 하나라도 찾은 질의 비율. `returned`: 반환 개수 평균. `read_tok`: 결과 span을 모두 읽는 비용(≈ 바이트/4).
- $K_{\max}=10$, 적응형 컷.

## 결과

**sharkdp/fd** (`ce97e47`, Rust, `--holdout 200 --max-commits 900`) — $T_0$ 색인 32 파일, 676 leaf, 457 edge

| method | n | recall | hit | returned | read_tok |
|---|---:|---:|---:|---:|---:|
| grep-read@1 text | 106 | 0.135 | 0.170 | 1.0 | 5806 |
| grep-read@3 text | 106 | 0.452 | 0.594 | 3.0 | 16489 |
| **ubis text** | 106 | 0.253 | 0.377 | 6.3 | 1235 |
| read-anchor-file | 60 | 0.437 | 0.483 | 1.0 | 3191 |
| **ubis anchor** | 60 | 0.322 | 0.417 | 2.5 | 899 |
| **ubis anchor+text** | 60 | 0.320 | 0.467 | 5.4 | 1134 |

**psf/requests** (`611c6162`, Python, `--holdout 150 --max-commits 700`) — $T_0$ 색인 69 파일, 2292 leaf, 2064 edge

| method | n | recall | hit | returned | read_tok |
|---|---:|---:|---:|---:|---:|
| grep-read@1 text | 55 | 0.244 | 0.273 | 1.0 | 12755 |
| grep-read@3 text | 55 | 0.469 | 0.509 | 3.0 | 32586 |
| **ubis text** | 55 | 0.154 | 0.218 | 6.8 | 654 |
| read-anchor-file | 20 | 0.464 | 0.500 | 1.0 | 5254 |
| **ubis anchor** | 20 | 0.264 | 0.300 | 3.2 | 944 |
| **ubis anchor+text** | 20 | 0.307 | 0.350 | 6.2 | 780 |

## 해석

- **토큰 효율은 크게 앞서지만 절대 recall은 아직 낮다.** fd에서 `ubis text`는 grep-read@1보다 recall이 높고 토큰은 1/5이다. grep-read@3은 recall이 더 높지만 토큰이 13배다. requests에서는 grep-read@1에도 recall이 뒤진다.
- **커밋 제목은 거친 질의다.** `v2.31.0`, `Fix linting issues`, `Pre commit update` 같은 제목은 어떤 방법으로도 답이 없다. text 모드 절대값은 이 영향을 크게 받는다. 방법 간 상대 비교로만 읽어야 한다.
- **anchor 모드에서는 `tree_near`(형제)가 대부분을 한다.** fd: 전체 0.322, `tree_near` 제거 0.081, `refs` 제거 0.275. 참조 엣지 기여는 +0.047. 같은 파일 안의 co-change가 많다는 뜻이고, `read-anchor-file`이 높은 이유와 같다.
- **`read-anchor-file`이 recall에서 이긴다**(0.437 vs 0.322, 토큰 3.5배). anchor 모드의 적응형 컷이 너무 이르게 자르는 것(평균 2.5개)이 한 원인으로 보인다. 다음 실험 대상.
- **anchor + 텍스트 가중치**: 텍스트를 1.0으로 두면 anchor 신호가 묻혔다(fd 0.254, requests 0.157). 0.25로 낮춰 fd 0.320 / requests 0.307. 이 값이 현재 planner 기본값이다.

## 변경 2: `same_file` operator, $K_{\min}=5$

anchor 모드가 평균 2.5개만 반환하고 `read-anchor-file`에 recall로 크게 졌다. 원인은 두 가지: 후보가 형제로만 제한됐고, 적응형 컷 하한(3)이 너무 낮았다.

| fd | 기존 | `same_file` 추가 | + $K_{\min}=5$ | ($K$=10 고정, 상한 참고) |
|---|---:|---:|---:|---:|
| anchor recall | 0.322 | 0.322 | **0.351** | 0.368 |
| anchor+text recall | 0.320 | 0.325 | **0.336** | 0.384 |
| text recall | 0.253 | 0.253 | 0.253 | 0.305 |

| requests (n=20, 잡음 큼) | 기존 | 현재 기본값 |
|---|---:|---:|
| anchor recall | 0.264 | 0.236 |
| anchor+text recall | 0.307 | **0.393** |
| text recall | 0.154 | 0.154 |

현재 기본값 전체 표 (fd / requests):

| method | fd recall | fd read_tok | requests recall | requests read_tok |
|---|---:|---:|---:|---:|
| grep-read@1 text | 0.135 | 5806 | 0.244 | 12755 |
| grep-read@3 text | 0.452 | 16489 | 0.469 | 32586 |
| read-anchor-file | 0.437 | 3191 | 0.464 | 5254 |
| ubis anchor | 0.351 | 1135 | 0.236 | 1034 |
| ubis anchor+text | 0.336 | 1261 | 0.393 | 889 |
| ubis text | 0.253 | 1282 | 0.154 | 665 |

- requests anchor-only가 0.264→0.236으로 내려갔다. 질의 20개라 한두 개 차이다. 코퍼스를 늘려야 판단할 수 있다.
- $K$=10 고정이 recall은 더 높다. 적응형 컷이 recall을 깎는 대신 토큰을 아낀다. 이 교환비를 어디에 둘지는 실제 에이전트 사용으로 정해야 한다.

## 다음 측정

1. anchor 모드 $K_{\min}$과 컷 규칙 조정 → `read-anchor-file`과의 recall 격차
2. co-change operator (hunk는 이미 저장됨) — 시간 분할 안에서 $T_0$ 이전 커밋만 사용
3. 코퍼스 확대: 코드 비중이 큰 레포 여러 개, 문서 위주 폴더, 한국어 문서
4. 커밋 제목 대신 이슈 본문 등 더 자연스러운 질의원

## 실험 E1: co-change operator

2026-09-28, macOS arm64. `ubis-bench --cochange <w>` (opt-in, 기본 plan 미포함).

- **가설:** 과거에 함께 바뀐 unit은 앞으로도 함께 바뀐다. anchor 모드 recall을 올린다.
- **정의:** $T_0$ 이전 커밋만 사용(누수 없음). 커밋 $c$의 unit 집합 $S_c$는 하네스 `touched_units`와 같은 로직(그 커밋 시점 파일 재파싱 → $T_0$ unit으로 매핑, 서수 unit은 Jaccard ≥ 0.3). $X_{ij}=\sum_{c:\,i,j\in S_c} e^{-(T_0-t_c)/\tau}/(|S_c|-1)$, support ≥ 2, $|S_c|\le 40$. 구현: `ubis-core/src/cochange.rs`.
- **코퍼스 추가:** `BurntSushi/ripgrep@3fce3b5`, `pallets/flask@d73fa1cd` (둘 다 `--holdout 200 --max-commits 900`, `--depth 900` 클론).

anchor recall / read_tok (anchor+text recall은 괄호):

| corpus (n) | read-anchor-file | 기존 | co-change w=0.5 τ=365d | w=0.8 | support=1 | τ=90d | τ=∞ |
|---|---:|---:|---:|---:|---:|---:|---:|
| fd (60) | 0.437 / 3191 | 0.351 / 1135 (0.336) | **0.470** / 1760 (0.451) | 0.487 | 0.462 (0.461) | 0.487 / 1581 | 0.453 |
| requests (20) | 0.464 / 5254 | 0.236 / 1034 (0.393) | **0.286** / 1094 (0.393) | 0.286 | 0.311 (0.461) | 0.286 | 0.236 |
| ripgrep (81) | 0.327 / 12047 | 0.090 / 2034 (0.141) | **0.253** / 4412 (0.292) | 0.265 | 0.285 (0.297) | 0.265 | 0.262 |
| flask (29) | 0.604 / 3778 | 0.387 / 2606 (0.492) | 0.380 / 2741 (0.485) | 0.380 | 0.380 (0.488) | 0.380 | 0.380 |

재현: `ubis-bench <repo> --holdout H --max-commits M --cochange 0.5 [--cochange-tau D] [--cochange-support S]`.

- **채택 기준 통과(anchor 모드):** fd +0.119, requests +0.050. ripgrep +0.163. flask −0.007(질의 1개 이하 차이, 중립).
- **fd에서 처음으로 `read-anchor-file`을 넘었다**(0.470 vs 0.437, 토큰 0.55배). ripgrep은 기존 anchor가 0.090으로 거의 작동하지 않았는데(다중 crate, 파일 간 co-change가 많음) 0.253까지 올라 격차가 크게 줄었다.
- **토큰 비용 증가:** anchor read_tok이 fd 1.5배, ripgrep 2.2배. 여전히 `read-anchor-file`의 1/2~1/3.
- **감쇠:** τ=∞(감쇠 없음)가 가장 약하다. 90d와 365d는 비슷. 기본은 365d.
- **support=1**은 requests anchor+text를 크게 올리지만(0.393→0.461, n=20) fd anchor는 내린다. 보류.
- **flask:** 효과 없음. `read-anchor-file`이 이미 0.604 — 파일 내 co-change 위주라 `same_file`이 이미 잡고 있는 것으로 보인다.

### 기본 plan 편입 (w=0.5, τ=365d, support ≥ 2)

- hunk → unit 매핑을 `ubis-ingest::history::UnitMapper`로 옮겨 하네스(정답·co-change)와 `ubis index --git`이 같은 코드를 쓴다. 파생 테이블 `cochange`에 저장하고 `CoChange` operator가 읽는다. 하네스는 이제 기본 plan을 그대로 측정한다(ablation: `--disable cochange`).
- **동등성 확인:** 4개 코퍼스에서 `--disable cochange` 결과가 편입 전 기본값과 모든 행·열에서 동일하고, 기본값은 위 w=0.5 열과 동일하다. 즉 매퍼 이전으로 정답 집합이 바뀌지 않았다.
- **속도** (macOS arm64, release):

| | 이전 | 이후 |
|---|---:|---:|
| co-change 구축, ripgrep 703커밋 (하네스) | 수십 초 (파일마다 `git show`) | 2.2s (`cat-file --batch` 1개 + (path, blob) 캐시 + 미색인 경로 생략) |
| co-change 구축, fd 614커밋 | — | 0.78s |
| 하네스 전체, ripgrep | 28.2s | 7.3s (grep baseline term 카운트를 파일당 1회로) |
| `ubis index --git .` ripgrep 첫 실행 (903커밋) | — | 4.3s |
| 같은 명령 재실행 (변경 없음) | 3.2s | 0.03s (`meta.git_basis` = 버전 + HEAD + 파일 해시가 같으면 생략) |
| `ubis near` 질의 | — | 9ms |
