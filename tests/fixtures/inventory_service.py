"""Inventory service: reservations, restocks and audit trail."""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from datetime import datetime, timedelta
from typing import Iterable, Optional

from django.db import transaction
from django.db.models import F

from shop.inventory.models import AuditEntry, Item, Reservation

log = logging.getLogger(__name__)

RESERVATION_TTL = timedelta(minutes=15)


class OutOfStock(Exception):
    """Raised when a reservation asks for more than is available."""


@dataclass
class ReservationRequest:
    sku: str
    qty: int
    customer_id: Optional[int] = None
    tags: list[str] = field(default_factory=list)


class InventoryService:
    """All stock mutations go through here."""

    def __init__(self, clock=datetime.utcnow, audit: bool = True):
        self.clock = clock
        self.audit = audit
        self._cache: dict[str, int] = {}

    def available(self, sku: str) -> int:
        """Units that can still be reserved."""
        if sku in self._cache:
            return self._cache[sku]
        item = Item.objects.only("available").get(sku=sku)
        held = (
            Reservation.objects.filter(item__sku=sku, expires_at__gt=self.clock())
            .values_list("qty", flat=True)
        )
        free = item.available - sum(held)
        self._cache[sku] = free
        return free

    def reserve_stock(self, req: ReservationRequest) -> Reservation:
        if req.qty <= 0:
            raise ValueError("qty must be positive")
        with transaction.atomic():
            item = Item.objects.select_for_update().get(sku=req.sku)
            if item.available < req.qty:
                raise OutOfStock(f"only {item.available} left of {req.sku!r}")
            item.available -= req.qty
            item.save(update_fields=["available"])
            reservation = Reservation.objects.create(
                item=item,
                qty=req.qty,
                customer_id=req.customer_id,
                expires_at=self.clock() + RESERVATION_TTL,
            )
        self._cache.pop(req.sku, None)
        self._record("reserve", req.sku, -req.qty, meta={"reservation": reservation.pk})
        return reservation

    def release(self, reservation: Reservation) -> None:
        """Return a reservation's units to the pool."""
        with transaction.atomic():
            Item.objects.filter(pk=reservation.item_id).update(
                available=F("available") + reservation.qty
            )
            reservation.delete()
        self._cache.pop(reservation.item.sku, None)
        self._record("release", reservation.item.sku, reservation.qty)

    def restock(self, sku: str, qty: int, *, reason: str = "delivery") -> int:
        if qty <= 0:
            raise ValueError("restock qty must be positive")
        with transaction.atomic():
            updated = Item.objects.filter(sku=sku).update(available=F("available") + qty)
            if not updated:
                Item.objects.create(sku=sku, available=qty)
        self._cache.pop(sku, None)
        self._record("restock", sku, qty, meta={"reason": reason})
        log.info("restocked %s by %d (%s)", sku, qty, reason)
        return self.available(sku)

    def expire_reservations(self, now: Optional[datetime] = None) -> int:
        now = now or self.clock()
        expired = Reservation.objects.filter(expires_at__lte=now).select_related("item")
        count = 0
        for reservation in expired.iterator():
            try:
                self.release(reservation)
                count += 1
            except Item.DoesNotExist:
                log.warning("reservation %s points at a deleted item", reservation.pk)
                reservation.delete()
        if count:
            log.info("expired %d reservations", count)
        return count

    def bulk_available(self, skus: Iterable[str]) -> dict[str, int]:
        result = {}
        missing = []
        for sku in skus:
            if sku in self._cache:
                result[sku] = self._cache[sku]
            else:
                missing.append(sku)
        for item in Item.objects.filter(sku__in=missing).only("sku", "available"):
            result[item.sku] = item.available
            self._cache[item.sku] = item.available
        for sku in missing:
            result.setdefault(sku, 0)
        return result

    def _record(self, action: str, sku: str, delta: int, meta: Optional[dict] = None) -> None:
        if not self.audit:
            return
        AuditEntry.objects.create(
            action=action,
            sku=sku,
            delta=delta,
            meta=meta or {},
            created_at=self.clock(),
        )


def low_stock_report(service: InventoryService, skus: Iterable[str], threshold: int = 5) -> str:
    levels = service.bulk_available(skus)
    rows = [f"{sku:<12} {qty:>5}" for sku, qty in sorted(levels.items()) if qty < threshold]
    if not rows:
        return "All SKUs above threshold."
    header = f"{'SKU':<12} {'QTY':>5}\n" + "-" * 18
    return "\n".join([header, *rows])
