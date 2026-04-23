"""원본 심볼 → Gate FX 심볼 정규화."""

from __future__ import annotations


class SymbolMap:
    """Config의 `[symbols.<source>]` 테이블을 래핑.

    매핑에 없는 심볼은 raw_symbol 그대로 통과시키고 warn 한번만 남긴다
    (Gate FX 포맷 확정 전엔 어차피 placeholder라 느슨하게).
    """

    def __init__(self, mapping: dict[str, str]) -> None:
        self._m: dict[str, str] = dict(mapping)
        self._warned: set[str] = set()

    def to_gate(self, raw: str) -> str:
        return self._m.get(raw, raw)

    def has(self, raw: str) -> bool:
        return raw in self._m
