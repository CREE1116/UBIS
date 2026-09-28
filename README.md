# UBIS

**Where, not what.** 에이전트에게 지도가 아니라 후보 목록을 준다.

UBIS는 내 PC의 텍스트(코드·문서)를 **unit**(함수, 메서드, 절, 문단) 단위로 색인해서, LLM 에이전트가 적은 호출과 적은 토큰으로 정확한 위치에 도달하게 하는 **결정론적 로컬 인덱스**다. 학습·LLM·신경망 임베딩을 쓰지 않는다.

```
$ ubis find "how do incremental removals rebuild the vector index"      # (이전 VectorDB 레포에서)
[1] crates/vectordb-core/src/hnsw.rs:126-135  crates/vectordb-core/src/hnsw.rs::HnswIndexThreadSafe::rebuild  (method)
    /// Rebuild after removals; HNSW cannot safely leave deleted nodes in search paths.
    via lexical 1.00
[3] crates/vectordb-core/src/engine.rs:586-607  crates/vectordb-core/src/engine.rs::VectorDBEngine::rebuild_vector_indices  (method)
    fn rebuild_vector_indices(&self, affected: &std::collections::HashSet<FeatureSpace>) {
    via lexical 0.52
```

## 핵심 아이디어

1. **그래프는 증거이고, 인덱스가 아니다.** 저장하는 것은 파서가 본 원본 증거(unit, 정의, 참조 mention, 링크, git hunk)뿐이다. 엣지는 `mentions ⋈ definitions`로 매번 유도되므로, 정의가 바뀌어도 낡은 엣지가 남지 않는다.
2. **검색과 이동은 같은 연산이다.** 모든 신호는 operator $\phi_f$이고 점수는 $s(j)=\sum_f w_f\,\phi_f(q,\text{anchor},j)$ 하나다. 텍스트만 주면 검색, anchor(지금 보고 있는 unit)를 주면 이동, 둘 다 주면 그 사이.
3. **정답이 아니라 후보를 준다.** 목표는 1000개를 10개로 줄이되 정답을 놓치지 않는 것. 모호한 참조는 버리지 않고 질량을 나눈다($m$개 후보면 각 $1/m$). 결과 수는 점수 분포로 적응적으로 자른다.
4. **Don't walk the graph. Project it.** (실험 트랙) relation 행렬의 닫힌 형태 스펙트럼 필터(EASE / Wiener)로 다중 홉 구조를 내적 하나로 투영한다. 코어가 아니라 operator로 꽂히며, `ubis-bench`에서 이겨야 기본 경로에 들어간다.

## 사용

```bash
cargo install --path crates/ubis-cli        # `ubis`
cargo install --path crates/ubis-bench      # `ubis-bench`

ubis index .                  # 증분 색인 (.ubis/index.db). 바뀐 파일만 재추출
ubis find "토큰 만료 처리"      # 텍스트 → 후보 unit
ubis find --anchor Store::open "error handling"   # anchor + 텍스트
ubis near Store::open         # anchor만: 참조하는 곳, 참조되는 곳, 형제
ubis refs Store::open         # 기록된 참조 전체 (origin, 질량 포함)
ubis open Store::open         # span 본문. --out 이면 부모, 컨테이너면 자식 목록
ubis status
```

모든 명령은 `--json`을 지원한다. unit 참조는 전체 ID(`src/store.rs::Store::open`), 라벨, `::` 접미사(`Store::open`) 중 하나로 줄 수 있다.

## 무엇을 색인하나

| 포맷 | unit | 정의 | mention |
|---|---|---|---|
| Rust, Python, Java (tree-sitter) | 모듈 › 타입/impl/클래스 › 함수/메서드, 남는 줄은 gap | 심볼 (+ `Owner::method`) | 호출, 타입 참조, import |
| Markdown | heading 계층 › 문단 / 코드 블록 | GitHub식 anchor | `[t](#a)`, `[t](x.md#a)`, `[[wiki]]`, 문장 속 식별자 |
| 텍스트 (`.txt`, `.tex`, `.rst`, 기타 코드) | 문단 | 파일 | 문장 속 식별자 |

읽을 수 있는 UTF-8 텍스트만 받는다. 바이너리·이미지·Office·PDF는 제외한다(PDF 텍스트 추출은 이후 extractor로 추가).

## 구조

```
crates/
  ubis-core    unit 모델, SQLite 증거 저장소, resolver, 질의 cascade, 토크나이저
  ubis-ingest  텍스트 판별, extractor(code/markdown/text), 증분 인덱서
  ubis-git     커밋·hunk 추출
  ubis-cli     `ubis`
  ubis-bench   시간 분할 평가 하네스
```

설계는 [DESIGN.md](DESIGN.md), 측정값은 [REPORT.md](REPORT.md).

## 개발

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
ubis-bench path/to/repo --holdout 200     # 평가
```

라이선스: MIT OR Apache-2.0
