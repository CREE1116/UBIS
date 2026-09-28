# UBIS 설계

## 목표

LLM 에이전트가 내 PC의 텍스트(문서·코드)에서 **최소 호출·최소 토큰으로 올바른 unit에 도달**하게 한다. 평가 기준은 에이전트의 기대 비용이다.

$$\mathbb{E}[\text{cost}] = K\cdot t_{\text{cand}} + \sum_{\text{열람}}|\text{span}| + P(\text{miss}\mid K)\cdot C_{\text{fallback}}$$

후보 하나는 싸고(경로·줄·시그니처 ≈ 30 tok), 놓치면 비싸다(grep + 파일 통째 읽기). 그래서 precision보다 recall, 파일보다 unit span.

제약: **결정론적 연산만.** 학습·LLM·신경망 임베딩 없음. 같은 입력이면 같은 인덱스, 같은 결과.

## 층

```
Interface   ubis CLI (find / near / refs / open / status), 에이전트 스킬
Query       Planner → Stage A 후보 생성 → Stage B 점수 → Stage C 성형
Derivation  Resolver (mentions ⋈ definitions → edges)   [실험: Factorizer]
Evidence    SQLite: files, units, postings, definitions, mentions, edges, commits, hunks
Ingest      walker(.gitignore) → 텍스트 판별 → extractor → 파일 단위 교체
```

**Evidence만 원천이다.** 엣지와 (향후) 벡터·그래프 view는 전부 파생물이며 언제든 재계산할 수 있다.

## Unit

- ID는 구조적 경로다: `src/store.rs::Store::open`, `docs/a.md#install/linux`, 이름 없는 unit은 부모 + 서수(`/¶3`, `/~2`, `/code1`). 줄 번호는 속성이다.
- 파일마다 트리를 이룬다. **색인은 leaf만** 하고, 컨테이너는 헤더 한 줄만 텍스트로 가진다.
- 컨테이너에서 이름 있는 자식이 덮지 않는 줄은 **gap leaf**가 된다(import, 필드, 자유 텍스트). 비어 있지 않은 모든 줄은 정확히 하나의 leaf에 속한다.
- 알려진 한계: 서수 ID는 위쪽에 텍스트가 삽입되면 밀린다. 해법은 Unit Diff(재파싱 시 내용 유사도로 이전 ID 승계)이며, 현재는 평가 하네스에서만 쓴다.

## 참조 해석 (Resolver)

- `link`: 명시적 문서 링크 → anchor 정의에 정확 매칭 (`origin=explicit`).
- `call`/`type`/`import`: 같은 파일 정의 우선(`same_file`), 없으면 같은 이름 전체(`global`).
- 타입 한정 호출(`Store::open()`, `Self::open()`, `self.open()`)은 `Owner::method` 정의에만 매칭한다. `Vec::new()`가 로컬 `new`로 새지 않는다. 모듈 경로(`bm25::f`)는 소문자 관례로 구분해 단순 이름으로 해석한다.
- `bridge`: 문장·백틱 속 식별자 → 다른 파일의 코드 심볼. 후보가 3개를 넘으면 버린다.
- 후보 $m$개면 각 $1/m$ 질량. 같은 (src, dst, kind)는 합산.

## 질의 cascade

| Operator | 신호 | 비고 |
|---|---|---|
| `lexical` | BM25 (leaf) | 코드 subword 분리, 한글 문자 bigram |
| `symbol` | 정의 이름 정확 일치, $1/n$ | 식별자 질의에서 강함 |
| `refs_in` / `refs_out` | anchor로 들어오는/나가는 엣지 질량 | 방향 있음 |
| `tree_near` | anchor 형제, 문서 순서 거리 $1/(1+d)$ | |

- **Planner**: 식별자형 질의 → `symbol` 가중; anchor 있음 → 관계 operator + 텍스트는 0.25로 낮춤(하네스로 측정해 정함).
- **Stage B**: $\phi_f = \text{raw}_f/\max\text{raw}_f$, $s=\sum_f w_f\phi_f$. 점수 간격을 보존해야 적응형 $K$가 의미 있다.
- **Stage C**: (1) 조상·자손이 함께 뜨면 자손만 남김 (2) 작은 부모(≤300줄) 밑 형제가 4개 이상이면 부모 하나로 올림 (3) $K\in[3,K_{\max}]$에서 최대 점수 낙차 지점, 단 상위 $K_{\max}$ 질량의 50% 이상 유지, 동률이면 큰 $K$.

## 평가 (ubis-bench)

git은 증거이자 채점자라서 **시간으로 자른다**. $T_0$ 시점 트리만 색인하고, 이후 커밋마다 변경 hunk와 unit span의 교집합을 정답으로 쓴다.

- `text`: 커밋 제목 → 변경 unit
- `anchor`: 변경 unit 하나 → 나머지
- `anchor+text`: 둘 다
- baseline: `grep-read@F` (질의어를 많이 포함한 파일 F개 통째 읽기), `read-anchor-file`

실험 operator는 여기서 recall 또는 토큰에서 이겨야 기본 plan에 들어간다. `--disable`, `--weight`로 ablation.

## 확장 지점

- **Extractor** (새 증거원): `extract(path, content) → {units, definitions, mentions}` 계약만 따르면 된다. 후보: config 키 경로(json/yaml/toml), LaTeX `\label`/`\ref`/`\cite`, PDF 텍스트, Go/TS/C++.
- **Operator** (새 점수 함수): `generate(store, query, ctx) → [(unit, raw)]`.

## 실험 트랙 (코어 아님)

1. **co-change operator**: $X^{co}_{ij}=\sum_c e^{-(T-t_c)/\tau}/(|S_c|-1)$, 커밋 $c$가 건드린 unit 집합 $S_c$. hunk는 이미 `index --git`으로 저장된다.
2. **term EASE 쿼리 확장**: $\tilde q = q\,G(G+\lambda I)^{-1}$ (low-rank). 신경망 없이 공출현 기반 동의어.
3. **relation factor**: $X_t\approx U S V^\top$, $u=S^{1/2}U$(바라봄), $v=S^{1/2}V$(바라봐짐). $s(A\to B)=\langle u_A,v_B\rangle$. co-citation은 EASE 형태
   $B_{ij}=\langle a_i,a_j\rangle/(1-\lVert a_j\rVert^2)$, $a=\mathrm{diag}\big(\sqrt{\lambda_k/(\lambda_k+\lambda)}\big)V^\top$ (정확한 등식).
4. **Unit Diff**: 재색인 시 서수 unit ID 승계.
5. **watch**: 파일 이벤트 → 해당 파일만 교체 + 엣지 재유도.
