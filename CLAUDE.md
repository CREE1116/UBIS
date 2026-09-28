# CLAUDE.md — UBIS 작업 가이드

이 파일은 이 레포에서 작업하는 에이전트(와 사람)를 위한 것이다. 무엇을 추구하는지, 무엇을 지켜야 하는지, 다음에 무엇을 할지를 적는다. 설계 상세는 [DESIGN.md](DESIGN.md), 측정값은 [REPORT.md](REPORT.md).

## 1. 무엇을 만드는가

**UBIS — Where, not what.** 내 PC의 텍스트(코드·문서)를 unit(함수·메서드·절·문단) 단위로 색인해서, LLM 에이전트가 **최소 호출·최소 토큰으로 올바른 unit span에 도달**하게 하는 결정론적 로컬 인덱스.

판단 기준은 하나다. 에이전트의 기대 비용:

$$\mathbb{E}[\text{cost}] = K\cdot t_{\text{cand}} + \sum_{\text{열람}}|\text{span}| + P(\text{miss}\mid K)\cdot C_{\text{fallback}}$$

- 후보는 싸고(≈30 tok), 놓치면 비싸다(grep + 파일 통째 읽기). → **precision보다 recall.**
- 파일 전체가 아니라 span을 돌려준다. → **unit 단위가 존재 이유.**
- 정답을 맞히는 게 아니라 1000개를 10개로 줄이는 것이 일이다.

캐치프레이즈: *Where, not what.* / *Not a map — a shortlist.* / *The graph is the evidence. The geometry is the index.* / *Don't walk the graph. Project it.*

## 2. 지켜야 할 원칙 (어기려면 먼저 사용자와 합의)

1. **결정론적 연산만.** 학습, LLM, 신경망 임베딩 금지. 같은 입력이면 비트 단위로 같은 인덱스·같은 결과. 출력에 `HashMap` 순회 순서가 새지 않게 정렬한다.
2. **Evidence만 원천.** SQLite의 `units / postings / definitions / mentions / files / commits / hunks`가 유일한 진실. `edges`와 앞으로 생길 벡터·factor는 전부 파생물이고 언제든 재계산 가능해야 한다. 파싱 시점에 엣지를 확정하지 않는다.
3. **텍스트만.** 읽을 수 있는 UTF-8만 색인. 멀티모달은 정확성과 모달리티 간 검색을 신뢰할 수 없어서 의도적으로 뺐다.
4. **모호함은 버리지 말고 질량으로 나눈다** ($m$개 후보면 각 $1/m$). 단, 에이전트에게 근거(`via`, `origin`)를 보여줘서 exact와 추정을 구분하게 한다.
5. **기본 경로는 하네스가 결정한다.** 새 operator·가중치·컷 규칙은 `ubis-bench`에서 **2개 이상 코퍼스**로 측정하고, recall 또는 토큰에서 이겨야 기본 plan에 들어간다. 결과는 좋든 나쁘든 REPORT.md에 기록한다.
6. **코어는 단단하게, 실험은 꽂아서.** 실험은 `Extractor` 또는 `Operator`로만 들어온다. 코어 스키마를 실험 때문에 바꾸지 않는다(필요하면 `factors` 같은 별도 테이블).

## 3. 코드 지도

```
crates/ubis-core/src/
  model.rs     Unit, Definition, Mention, Edge, Extracted (extractor 출력 계약)
  store.rs     SQLite 스키마, 파일 단위 교체, canonical_dump (동등성 테스트용)
  resolve.rs   mentions ⋈ definitions → edges (same_file > global, Owner::method, bridge ≤3)
  cochange.rs  commits/hunks → co-change 행렬(파생 테이블 `cochange`), CoChange operator
  query.rs     Operator trait, 8개 operator(+path), planner, Stage B/C, adaptive_k
  tokenize.rs  코드 subword, 영어 어간(+원형), 한글 bigram; tokenize_raw(어간 없음, 채점용)
crates/ubis-ingest/src/
  lib.rs       admit(텍스트 판별), extract(디스패치), walk, index_dir, index_paths
  history.rs   History(blob 파싱 캐시) / UnitMapper(hunk → 현재 unit), record_and_derive: 이력 기록 → store에서 co-change 파생
  tree.rs      UnitTree: ID 중복 처리, gap leaf, leaf/컨테이너 텍스트 규칙
  code.rs      tree-sitter Rust/Python/Java
  markdown.rs  heading 계층, GitHub anchor, 링크, 위키 링크
  text.rs      문단 + bridge
  prose.rs     bridge mention 추출, 문단 분할
crates/ubis-git/   git log -p --raw --unified=0 파싱(hunk + blob id), BlobReader(cat-file --batch), archive
crates/ubis-cli/   ubis: index / watch / find / near / refs / open / status
crates/ubis-bench/ 시간 분할 평가 (text / anchor / anchor+text / find->near, grep-read baseline), --tasks PR 태스크 모드
  scripts/fetch_pr_tasks.py  gh로 병합 PR → 태스크 JSONL
integrations/skills/ubis/SKILL.md   에이전트용 사용법
```

**확장 지점**
- 새 포맷 → `ubis-ingest`에 extractor 추가, `extract()` 디스패치에 연결. `UnitTree`를 쓰면 gap·ID 규칙이 자동으로 맞는다.
- 새 신호 → `query.rs`에 `impl Operator`, `plan()`에서 가중치와 함께 등록. 하네스에서는 `--disable name`, `--weight name=w`로 ablation.

## 4. 불변식과 테스트

- `incremental_equals_fresh`: 여러 단계 편집(정의 이름 변경, 동명 정의 추가, 삭제, 바이너리 전환) 후 증분 인덱스 == 새 인덱스 (`canonical_dump` 비교).
- `index_paths_equals_fresh`: watcher 경로(디렉터리 rename 포함)도 동일.
- `cochange_from_history`: 실제 git 레포에서 hunk→unit 매핑(서수 unit 이동 포함)과, co-change 재계산이 같은 증거에서 같은 dump를 내는지.
- 새 기능이 저장 내용을 바꾸면 이 두 테스트에 시나리오를 추가한다.
- 커밋 전: `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings`.

## 5. 작업 방법

```bash
cargo build --release
./target/release/ubis find "..."        # 첫 호출이 색인을 만들고, 이후 자동 갱신
./target/release/ubis-bench <repo> --holdout 200 --max-commits 900          # 기본
./target/release/ubis-bench <repo> --disable same_file                      # ablation
./target/release/ubis-bench <repo> --weight lexical=0.5 --k-min 10 -v       # 가중치, 컷, 질의별 출력
```

지금까지 쓴 코퍼스: `sharkdp/fd@ce97e47` (Rust, `--holdout 200 --max-commits 900`), `psf/requests@611c6162` (Python, `--holdout 150 --max-commits 700`), `BurntSushi/ripgrep@3fce3b5`, `pallets/flask@d73fa1cd` (둘 다 `--holdout 200 --max-commits 900`). PR 태스크: `fetch_pr_tasks.py`로 네 레포 각각 생성 후 `--tasks … --max-commits 900`. **커밋 제목 모드보다 PR 태스크 모드를 우선 지표로 본다**(실제 작업 질의). 클론은 `--depth 900` 정도면 된다. 기본값을 바꾸면 두 코퍼스 결과를 REPORT.md에 표로 남긴다.

## 6. 현재 상태 (v0)

**된 것:** unit 트리와 구조적 ID, gap leaf, SQLite 증거 저장소, 증분 색인(stat 캐시로 미변경 파일은 읽지 않음), 매 질의 전 자동 갱신(파일 항상, git 이력은 HEAD가 움직일 때), git 레포에서 첫 호출 시 자동 색인, `path:line` anchor, watch, mention→edge 해석(질량 분할, 타입 한정 호출, 수신 타입 모르는 메서드 호출은 외부 몫 1 예약), BM25 + symbol + path(E7) + refs + tree_near + same_file + co-change(E1), 적응형 $K$, 시간 분할·PR 태스크 하네스(하네스와 CLI가 같은 hunk→unit 매핑 코드를 쓴다).

**설계 선호: 사용자와 에이전트가 알아야 할 것을 최소로.** 명령 두 개(`find`, `near`)면 되게 한다. 수동 단계(색인)는 자동으로 대체하고, 기각된 실험은 플래그로 남기지 말고 코드에서 지운다(기록은 REPORT.md). 새 인터페이스(MCP 등)는 스킬 + CLI로 안 될 때만.

**측정으로 확인된 것** (REPORT.md):
- **PR 태스크(실제 작업 질의)**: `find` 1 call이 fd·ripgrep·requests에서, `find->near` 2 call이 네 코퍼스 모두에서 grep → 파일 3개 읽기보다 recall이 높다(find->near: fd 0.469, ripgrep 0.366, requests 0.413, flask 0.336 vs grep 0.373/0.250/0.278/0.327). 총 토큰(목록+읽기) 1/10~1/29.
- **비용은 총량으로 본다:** 벤치의 `total_tok` = span 읽기 + `ubis` 출력 목록, `calls` = 도구 호출 수. 도구 자체의 출력·호출이 비용을 늘리지 않게 하는 것이 목적이다. 목록은 총비용의 ~10%, 읽기가 ~90%.
- 기각 기록(E11): 한 번에 find→near 확장(효율 나쁨), PRF(query drift), lift 축소(recall 손실).
- **채점은 고정:** 벤치의 정답 매핑과 grep baseline은 `tokenize_raw`. 검색 쪽 토크나이저를 바꿔도 정답 집합이 움직이지 않는다.
- text: fd에서 grep-read@1보다 recall 높고 토큰 1/5. 절대 recall은 낮다(0.25).
- anchor: 대부분 `tree_near`/`same_file`이 한다. 참조 엣지 기여는 작다(+0.05).
- `read-anchor-file`(파일 통째)이 recall에서는 아직 이긴다(0.44 vs 0.35, 토큰 약 3배).
- anchor가 있을 때 텍스트 가중치를 낮춰야 한다(0.25).

**알려진 한계**
- 서수 unit ID(`¶n`, `~n`, `codeN`)는 위쪽 삽입에 밀린다. 인덱서에는 Unit Diff가 없다(하네스에만 Jaccard 매핑 있음).
- 엣지 재유도가 전체 재계산이다(파일 하나 바뀌어도 전체 mention 조인). 개인 규모에선 괜찮지만 증분화 대상.
- tree-sitter는 Rust/Python/Java만. 나머지 코드는 문단 분할 텍스트로 들어간다.
- PDF, config 키 경로, LaTeX 구조 미지원.
- 커밋 제목은 거친 질의라 text 모드 절대값은 신뢰도가 낮다.

## 7. 다음 작업 (우선순위 순)

1. **실사용**: 사용자 PC에서 실제 폴더에 `ubis watch` + 에이전트 스킬로 돌려보고 실패 사례를 모은다. 하네스 수치보다 이게 먼저다.
2. **Unit Diff (인덱서)**: 재파싱 시 서수 unit에 이전 ID를 승계. 규칙은 경로 같으면 동일 → content hash 같으면 이동 → 같은 부모 안 Jaccard ≥ τ → 신규. co-change와 factor 실험의 전제 조건이다.
3. **증분 resolver**: 정의 이름 변화 집합 $N_\Delta$를 구해 `mentions WHERE name ∈ N_Δ`만 재해석. 불변식 테스트로 전체 재계산과 동일함을 보인다.
4. **extractor 확장**: LaTeX(`\section`, `\label`/`\ref`/`\eqref`/`\cite` = 명시 relation), config 키 경로(json/yaml/toml, 키 경로 = definition), TS/Go/C++ tree-sitter, PDF 텍스트.
5. **`map(scope)` 명령**: 디렉터리별 상위 unit 요약(in-degree 순). 처음 보는 폴더 탐색을 한 번의 호출로.
6. **코퍼스 확대**: 코드 위주 레포 3~5개, 문서 위주 폴더, 한국어 문서. 질의 수가 적은 코퍼스(n<50)의 결론은 보류한다.

## 8. 실험 트랙

각 실험은 **가설 → operator/변경 → 하네스 지표 → 채택 기준** 순으로 진행하고 REPORT.md에 결과를 남긴다. 채택 기준의 기본값: 두 코퍼스 모두에서 해당 모드 recall +0.02 이상, 또는 recall을 유지하면서 read_tok −20% 이상.

### E1. co-change operator — **채택됨** (REPORT.md, anchor일 때 가중치 0.5)
- **가설:** 과거에 함께 바뀐 unit은 앞으로도 함께 바뀐다. anchor 모드 recall을 올린다.
- **정의:** 커밋 $c$가 건드린 unit 집합 $S_c$에 대해 $X^{co}_{ij}=\sum_{c:\,i,j\in S_c} e^{-(T-t_c)/\tau}/(|S_c|-1)$. support ≥ 2.
- **주의:** 하네스에서는 $T_0$ 이전 커밋만 써야 누수가 없다. hunk → unit 매핑은 그 커밋 시점 파일을 재파싱해서 한다(하네스의 `touched_units`와 같은 로직). Unit Diff가 먼저 있으면 서수 unit도 안정적이다.
- **비교:** `read-anchor-file`을 넘는가.

### E2. term EASE 쿼리 확장 (텍스트 모드의 semantic)
- **가설:** 어휘가 다른 질의(`refresh` vs `renew`)를 공출현 구조로 잇는다.
- **정의:** $G = X_{\text{term}}^\top X_{\text{term}}$, $\tilde q = q\,F$, $F=G(G+\lambda I)^{-1}=V\,\mathrm{diag}\big(\tfrac{\lambda_k}{\lambda_k+\lambda}\big)V^\top$ (top-$k$ 고유쌍, Lanczos). 확장 term은 원 질의보다 낮은 가중치.
- **결정론:** Lanczos 시작 벡터 고정, 고유벡터 부호 규약(절댓값 최대 성분 양수), 합산 순서 고정. 비교는 벡터가 아니라 점수로.
- **저장:** `factors(epoch, block, id, side, vec)` 같은 별도 테이블. 코어 스키마는 건드리지 않는다.

### E3. relation factor: 방향 있는 "바라봄"
- **가설:** 다중 홉 참조 구조가 anchor 이웃을 늘린다(직접 엣지가 없는 unit까지).
- **정의:** relation type $t$마다 $X_t\approx U S V^\top$, $u=S^{1/2}U$(바라봄), $v=S^{1/2}V$(바라봐짐). $s(A\to B)=\langle u_A, v_B\rangle$, 역방향은 $\langle v_A, u_B\rangle$. 행 정규화 $p(B\mid A)=s_+/\sum_j s_+$로 해석 가능하게.
- **co-citation (EASE):** $B_{ij}=\langle a_i,a_j\rangle/(1-\lVert a_j\rVert^2)$, $a_j=\mathrm{diag}\big(\sqrt{\lambda_k/(\lambda_k+\lambda)}\big)V_{j,:}^\top$. 대각 제약 EASE와의 정확한 등식이다(full rank). $\lVert a_j\rVert^2$는 ridge leverage score이고, EASE의 비대칭은 이 스칼라에서만 온다. 링크 방향은 $u$/$v$에서만 나온다.
- **fold-in:** 새 unit은 $v_j=S^{-1}U^\top X_{:,j}$. drift $\lVert X_\Delta\rVert_F/\lVert X\rVert_F$가 임계를 넘으면 재분해.
- **주의:** 현재 참조 엣지 기여가 작다(+0.05). E3의 상한은 입력 $X$ 품질에 묶여 있다. 먼저 resolver 정확도를 확인할 것.

### E4. 적응형 컷 규칙
- 지금은 최대 낙차 + 질량 50% + $K_{\min}=5$. 대안: 누적 질량 $\tau$만 쓰기, 모드별 $K_{\min}$(identifier 질의 3, anchor 8), 점수 엔트로피 기반.
- 지표: recall과 read_tok의 파레토 곡선. $K$ 고정 상한과의 격차.

### E7. path operator — **채택됨** (제목 term ↔ 파일 경로, lexical × 0.5)
### E8. refs 차수 할인, E9. noisy-OR 엣지 결합 — 기각 (코드 제거, 기록은 REPORT.md)

### E5. Planner 라우팅
- 질의 형태 판정(식별자형 / 자연어 / anchor 유무)에 따른 $w$ 조정. 한국어 질의, 경로형 질의(`src/..`), 에러 메시지형 질의 규칙 추가.

### E6. 질의원 개선 — PR 태스크 모드 구현됨 (`--tasks`). 남은 것: 이슈 본문, 테스트 이름 → 구현 unit
- 커밋 제목 대신 PR 본문, 이슈 제목, 테스트 이름 → 구현 unit, 문서 절 → 식별자 다리 unit 등 더 자연스러운 질의·정답 쌍.

## 9. 하지 않을 것

- HNSW 같은 ANN 인덱스: 개인 규모에선 전수 계산이 더 빠르고 정확하다(이전 VectorDB에서 측정: 100k에서 전수 3.7ms, HNSW recall 0.48).
- 해시 dense 벡터: BM25의 잡음 섞인 복제본이었다.
- 스냅샷 파일 통째 저장: SQLite 행 단위 교체로 대체했다.
- 신경망 임베딩, LLM 추출, 멀티모달.

## 10. 관례

- 문서는 한국어, 코드 주석과 식별자는 영어.
- 수식은 LaTeX(`$...$`, `$$...$$`).
- 수치를 주장하면 재현 명령과 코퍼스 revision을 같이 적는다. 나쁜 결과도 기록한다.
- 이전 프로젝트 VectorDB(`CREE1116/VectorDB`)는 아카이브이자 설계 변경의 기록이다. 코드를 가져올 때는 여기 원칙에 맞게 다시 쓴다.
