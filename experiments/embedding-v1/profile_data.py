"""Shared executable constants for the cfetch embedding-v1 experiments."""

from __future__ import annotations

from typing import NamedTuple


QUERY_PREFIX = "task: search result | query: "
DOCUMENT_PREFIX = "title: none | text: "
DIMENSIONS = 768
SEQUENCE_BUCKETS = (32, 64, 128, 256, 512, 1024, 2048)

BASE_TEXTS = (
    "How does a content-addressed vector store reject conflicting derived artifacts?",
    "The daemon watches Markdown files, segments changed documents, and commits each derived record atomically.",
    "# Retrieval policy\nPrefer an exact cited passage, then widen to semantic recall when lexical evidence is incomplete.",
    'fn cosine_dot(left: &[i8], right: &[i8]) -> i64 { left.iter().zip(right).map(|(a, b)| i64::from(*a) * i64::from(*b)).sum() }',
    "SELECT block_hash, vector FROM embeddings WHERE profile_id = ?1 ORDER BY block_hash;",
    "git status --short --branch\nM src/embed.rs\n?? experiments/embedding-v1/",
    '{"network_major":1,"dimensions":768,"precision":"int8","compatible":true}',
    "Warum müssen alle Teilnehmer eines Netzes dasselbe Einbettungsprofil verwenden?",
    "Pourquoi un changement de modèle exige-t-il de recalculer toutes les représentations vectorielles ?",
    "¿Cómo se comprueba que dos aceleradores producen artefactos compatibles?",
    "同じ文書から生成されたベクトルは、共有する前にバイト単位で検証されます。",
    "同一模型版本、分词器、提示词和量化参数共同定义向量的含义。",
    "يجب أن تتطابق هوية النموذج وخط المعالجة قبل تبادل المتجهات بين الأجهزة.",
    "A failed accelerator conformance test makes that runner a consumer; it may not publish vectors.",
    "The query prompt and document prompt are intentionally different because retrieval training distinguishes their roles.",
    "Never reinterpret an old vector file in place. Create a new major namespace and re-embed deliberately.",
)


def calibration_text(index: int, target_tokens: int) -> str:
    prefix = QUERY_PREFIX if index % 4 == 0 else DOCUMENT_PREFIX
    base = BASE_TEXTS[index % len(BASE_TEXTS)]
    separator = "\n\n" if index % 3 else "\n"
    # Tokenization truncates this deterministic overestimate to target_tokens.
    repeat = max(1, target_tokens // max(4, len(base.split())) + 2)
    return prefix + separator.join([base] * repeat)


class KnownAnswer(NamedTuple):
    label: str
    kind: str
    text: str
    expected_bucket: int


_KAT_SEEDS = (
    (
        "short-query",
        "query",
        "Which files define cfetch's embedding compatibility boundary?",
        1,
        32,
    ),
    (
        "profile-document",
        "document",
        "The embedding profile pins the model, tokenizer, prompts, pooling, dimensions, quantization, and vector codec.",
        1,
        32,
    ),
    (
        "source-code",
        "document",
        'fn main() { println!("deterministic vectors"); }',
        1,
        32,
    ),
    (
        "german-query",
        "query",
        "Wie werden inkompatible Vektoren im Netzwerk verhindert?",
        1,
        32,
    ),
    (
        "japanese-document",
        "document",
        "同じコンテンツハッシュに異なるベクトルが届いた場合、保存を拒否します。",
        1,
        32,
    ),
    (
        "bucket-64",
        "document",
        "The canonical vector store rejects conflicting bytes for identical content.",
        3,
        64,
    ),
    (
        "bucket-128",
        "document",
        "fn verify(hash: &str, vector: &[i8]) { assert_eq!(vector.len(), 768); }",
        2,
        128,
    ),
    (
        "bucket-256",
        "query",
        "Warum müssen alle Teilnehmer genau dasselbe Einbettungsprofil verwenden?",
        9,
        256,
    ),
    (
        "bucket-512",
        "document",
        "同じコンテンツハッシュに異なるベクトルが届いた場合、保存を拒否します。",
        14,
        512,
    ),
    (
        "bucket-1024",
        "document",
        "يجب أن تستخدم جميع الأجهزة النموذج نفسه وخط المعالجة نفسه حتى تبقى المتجهات قابلة للتبادل.",
        19,
        1024,
    ),
    (
        "bucket-2048",
        "document",
        '{"network_major":1,"dimensions":768,"precision":"int8","compatible":true}',
        45,
        2048,
    ),
)


KAT_CASES = tuple(
    KnownAnswer(label, kind, "\n".join([seed] * repeats), expected_bucket)
    for label, kind, seed, repeats, expected_bucket in _KAT_SEEDS
)
