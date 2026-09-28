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

## 다음 측정

1. anchor 모드 $K_{\min}$과 컷 규칙 조정 → `read-anchor-file`과의 recall 격차
2. co-change operator (hunk는 이미 저장됨) — 시간 분할 안에서 $T_0$ 이전 커밋만 사용
3. 코퍼스 확대: 코드 비중이 큰 레포 여러 개, 문서 위주 폴더, 한국어 문서
4. 커밋 제목 대신 이슈 본문 등 더 자연스러운 질의원
