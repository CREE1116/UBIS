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

## E6: 실제 작업(PR) 질의 — `--tasks`

커밋 제목은 거친 질의다. 에이전트가 실제로 받는 것은 "이 이슈를 고쳐라"에 가깝다. 그래서 병합된 GitHub PR을 태스크로 쓴다: 질의 = PR 제목 + 본문, 색인 = PR 직전 트리(`base`), 정답 = `base..head`가 바꾼 unit, co-change = `base`까지의 이력(최근 900커밋). bot(dependabot 등) PR은 제외.

```bash
python3 crates/ubis-bench/scripts/fetch_pr_tasks.py sharkdp/fd <clone> tasks_fd.jsonl   # gh 필요
ubis-bench <clone> --tasks tasks_fd.jsonl --max-commits 900
```

PR 목록은 2026-09-28 기준 `gh pr list --state merged --limit 200`이고, 병합 커밋이 `--depth 900` 클론 안에 있는 것만 쓴다. PR 본문은 제3자 텍스트라 태스크 파일은 커밋하지 않고 스크립트만 둔다.

새 방법 **`ubis find->near`**: 에이전트의 2-call 흐름. `find(text)` 후 1위 unit을 anchor로 `find(text, anchor)`, 두 목록의 합집합을 읽는다.

태스크 모드는 store 하나를 태스크마다 증분 재색인하고 `History`(blob 파싱 캐시)를 공유한다. fd 90태스크 91s → 19s.

## E7: `path` operator — 채택

- **가설:** PR/커밋 제목의 scope(`printer: …`, `ignore/types: …`)는 파일 경로를 가리킨다. ripgrep 실패 사례 다수가 이 형태였다(`printer: fix --stats for --json` → `crates/printer/src/json.rs`).
- **정의:** 질의 첫 줄의 term을 파일 경로 토큰에 IDF(파일 단위 df)로 매칭 → 상위 5개 파일. 그 파일에서 질의 term을 포함한 leaf가 파일 점수를 받는다. 가중치 = lexical × 0.5(텍스트 0.5, anchor 0.125). 스키마 변경 없음(질의 시점 계산).
- **결과 (PR 태스크, find->near recall / read_tok):**

| corpus (n) | grep-read@3 | 이전 | **path** | (w=1.0) | (본문까지 매칭) |
|---|---:|---:|---:|---:|---:|
| fd (67) | 0.373 / 24304 | 0.388 / 2685 | 0.385 / 2940 | 0.391 | 0.412 |
| ripgrep (178) | 0.250 / 59062 | 0.178 / 3039 | **0.287** / 3394 | 0.341 | 0.273 |
| requests (114) | 0.278 / 41214 | 0.277 / 1844 | **0.345** / 1908 | 0.328 | 0.358 |
| flask (140) | 0.327 / 35673 | 0.265 / 2313 | **0.309** / 2101 | 0.288 | 0.304 |

text(1 call) recall: fd 0.370→0.366, ripgrep 0.157→0.238, requests 0.239→0.305, flask 0.229→0.270.

- **커밋 제목 모드 text recall** (`--disable path` 대비): fd 0.253→0.272, requests 0.150→0.181, ripgrep 0.214→0.316, flask 0.171→0.225. 네 코퍼스 모두 개선.
- fd는 중립(−0.004). w=1.0은 ripgrep에 더 좋지만 flask를 깎는다. 제목만 매칭이 4개 중 3개에서 낫다.
- **현재 기본값으로 PR 태스크에서:** `find->near`가 fd·requests·ripgrep에서 grep-read@3와 recall이 같거나 높고(ripgrep 0.287 vs 0.250), 토큰은 1/8~1/22. flask만 0.309 vs 0.327로 뒤진다(토큰 1/17).

## E8: refs 차수 할인(specificity) — 기각

- **가설:** 누구나 참조하는 대상(`Config`, `as_ref`)은 anchor에 대해 정보가 적다. `refs_out`은 $1/(1+\ln(1+\text{indeg}))$, `refs_in`은 out-degree로 할인. 동률(`refs_out 0.80` 7개가 ID 순)도 깬다.
- **결과:** 커밋 anchor recall fd 0.470→0.470, requests 동일, ripgrep 0.253→0.272, flask 0.368→0.418. anchor+text ripgrep −0.008. PR 태스크 anchor ±0.006. ripgrep anchor+text 토큰 −27%.
- 두 코퍼스 +0.02 기준을 flask 하나만 넘는다. 기각. (v0.3에서 꺼진 플래그로 남기지 않고 코드에서 제거.) 동률 문제 자체는 남아 있다.

## 구조 정리 (동작 불변 확인)

- **co-change를 기록된 증거에서 파생:** `commit_blobs(commit_id, path, blob)` 테이블 추가, `CommitRow` 구조체. `History::record_and_derive`가 이력을 먼저 기록하고 store에서 다시 읽어 co-change를 계산한다. git은 blob 내용을 읽을 때만 쓴다(content-addressed). 4개 코퍼스 수치 동일. `cochange-v2`로 basis 무효화.
- **`History` / `UnitMapper` 분리:** 파싱 캐시((path, blob) → leaf)는 index와 무관하게 재사용, 매핑 캐시는 index 상태별. 리뷰에서 지적한 "캐시 항목 꺼냈다 넣기"도 사라졌다.
- **서수 판정을 `UnitKind::is_ordinal()`로:** ID 문자열 파싱 제거. 파일 전체가 leaf 하나인 unit이 이제 ID로 정확히 매칭된다 → requests/flask 정답 집합이 약간 바뀜(flask 커밋 anchor n 29→30, recall 0.387→0.374, requests text 0.154→0.150). 이후 표는 모두 새 정의 기준.
- **`find --anchor`도 `near`와 같은 해석:** 모호한 이름이면 후보를 나열하고 실패한다(전에는 조용히 첫 후보).
- **`via` 표시:** 기여 < 0.005는 점수엔 포함, 출력에서 제외.

## Dogfooding: 이 세션의 실제 수정 5건

수정 **전** 트리(`cc98b0f`)를 색인하고, 각 수정을 작업 설명으로 물었다(순환 방지).

| 작업 설명 | 결과 |
|---|---|
| find --anchor가 모호한 이름에서 첫 후보를 조용히 고름 | `find` 2위 `resolve_unit`(재사용할 함수) → `near resolve_unit` 1위 `main`(실제 수정 위치) |
| 파일 경로와 질의어를 매칭하는 operator 추가 | `plan`, `Symbol`, `Lexical::generate` — 등록 위치와 본보기 |
| co-change를 기록된 이력에서 파생 | `replace_history`, `rebuild_cochange`, 테스트 적중. git 파서(`parse_log`)는 놓침 |
| via 0.00 숨기기 | `Via`, `print_hits` → `near Via` 2위 `search_with`(실제 수정 위치) |
| 과거 파일 파싱 캐시가 태스크 사이에 사라짐 | **실패.** 설명 어휘(reparse, cache, versions)와 코드 어휘(`UnitMapper`, `blob`, `leaves`)가 다름. 코드 어휘로 물으면 1위 |

관찰: 1 call로 절반, 2 call(`find→near`)로 4/5. 남은 실패는 어휘 불일치 — E2(term 확장)의 동기. 또 `x.iter()` 같은 std 메서드 호출이 로컬 `CoChangeIndex::iter`로 해석되는 잡음이 `near`에 보인다(수신 타입을 모르는 메서드 호출의 global 해석).

## v0.3: 알아야 할 것을 줄이기

원칙: 사용자와 에이전트가 쓸데없이 많은 것을 알 필요가 없어야 한다. 에이전트가 알아야 하는 것은 `find`와 `near` 두 명령뿐이다.

- **자동 갱신:** 모든 질의 명령이 답하기 전에 증분 색인한다. 파일은 항상, git 이력은 `HEAD`가 움직였을 때만 다시 파생한다. 명시적 `ubis index`는 파일이 바뀌면 이력도 재파생해서(정확 모드) 새로 만든 인덱스와 같은 결과를 보장한다.
- **stat 캐시:** `stat_cache(path, size, mtime_ns, checked_ns)`. 크기·mtime이 같고 마지막 확인보다 2초 이상 전에 수정된 파일은 읽지 않는다(git의 racy-clean 규칙과 같은 발상). 증거가 아니라 읽기 생략용 캐시이며 내용 해시가 여전히 기준이다. `incremental_equals_fresh`(2초 안에 연속 편집)가 그대로 통과한다.
- **첫 호출 자동 색인:** git 레포 안에서 인덱스가 없으면 레포 루트에 만든다. `.ubis/.gitignore`(`*`)로 `git status`를 오염시키지 않는다. git 밖에서는 `ubis index <dir>`를 안내하고 멈춘다(홈 디렉터리 전체 색인 같은 사고 방지).
- **`path:line` anchor:** `ubis near src/walk.rs:620` → 그 줄을 포함하는 가장 작은 unit. 경로는 인덱스 루트 기준, 현재 디렉터리 기준, 절대 경로 모두 받는다.
- **`--git` 기본값:** git 레포면 항상 이력을 쓴다(`--no-git`으로 끔).
- **MCP 서버는 만들지 않음:** 스킬 + CLI로 충분하고, 자동 갱신으로 스킬 쪽의 주된 불편(색인 단계 기억)이 없어졌다. 인터페이스를 하나 더 두는 비용이 더 크다.
- **`watch` 단순화:** 이벤트가 오면 `refresh()` 하나로 처리한다. `.git` 변화(커밋, checkout)도 이력 재파생으로 이어진다.

측정 (ripgrep, 221 파일 / 903 커밋, macOS arm64): 첫 호출 4~5s, 이후 갱신 오버헤드 10~20ms(`find` 전체 0.02~0.03s, `--no-refresh` 0.01s).

## E9: 메서드 호출의 외부 몫, noisy-OR

**외부 몫 — 채택(정합성).** 수신 타입을 모르는 호출(`x.len()`, Python `obj.f()`, Java `x.f()`)을 새 mention 종류 `method`로 기록하고, 후보 $m$개에 "프로젝트 밖" 1개를 더해 $1/(m+1)$씩 나눈다. 원칙 4(모호함은 질량으로)를 그대로 적용한 것이다. 벤치 영향은 중립이다: 커밋 모드 anchor+text fd 0.452→0.457, ripgrep 0.295→0.302, 나머지 ±0.002. PR 태스크 동일(ripgrep anchor+text 0.350→0.348). 회귀 없이 잘못된 확정 엣지를 줄이므로 채택.

**noisy-OR — 기각.** 위 수정 후에도 `resolve_unit`의 `.iter()` 두 번이 0.5+0.5=1.0으로 합산되어 확정 참조와 동률이었다. 같은 엣지의 여러 mention을 $1-\prod(1-w_i)$로 합치면 [0,1] 확률처럼 된다. 결과: ripgrep anchor +0.012(커밋·PR 모두), PR 태스크 ripgrep anchor+text **−0.025**, 나머지 동일. 기준 미달로 기각하고 코드에서 제거했다. 합산은 "여러 번 부르는 대상이 더 관련 있다"는 신호도 담고 있어서, 이를 버리면 anchor+text에서 손해를 보는 것으로 보인다.
