import { EventEmitter } from "node:events";
import type { Db, Tx } from "./db";
import { money, type Money } from "./money";

export type OrderStatus = "pending" | "paid" | "shipped" | "cancelled";

export interface LineItem {
  sku: string;
  qty: number;
  unitPrice: Money;
}

export interface Order {
  id: string;
  customerId: string;
  items: LineItem[];
  status: OrderStatus;
  createdAt: Date;
}

export class OrderError extends Error {
  constructor(message: string, readonly code: "EMPTY" | "NOT_FOUND" | "BAD_STATE") {
    super(message);
  }
}

export class OrderService extends EventEmitter {
  constructor(private readonly db: Db, private readonly taxRate = 0.2) {
    super();
  }

  async create(customerId: string, items: LineItem[]): Promise<Order> {
    if (items.length === 0) {
      throw new OrderError("an order needs at least one item", "EMPTY");
    }
    const order: Order = {
      id: crypto.randomUUID(),
      customerId,
      items,
      status: "pending",
      createdAt: new Date(),
    };
    await this.db.tx(async (tx: Tx) => {
      await tx.insert("orders", order);
      for (const item of items) {
        await tx.insert("order_items", { orderId: order.id, ...item });
      }
    });
    this.emit("created", order);
    return order;
  }

  async get(id: string): Promise<Order> {
    const row = await this.db.one<Order>("SELECT * FROM orders WHERE id = $1", [id]);
    if (!row) {
      throw new OrderError(`order ${id} not found`, "NOT_FOUND");
    }
    row.items = await this.db.many<LineItem>("SELECT * FROM order_items WHERE order_id = $1", [id]);
    return row;
  }

  total(order: Order): Money {
    let subtotal = money(0);
    for (const item of order.items) {
      subtotal = subtotal.add(item.unitPrice.times(item.qty));
    }
    const tax = subtotal.times(this.taxRate).round();
    return subtotal.add(tax);
  }

  async transition(id: string, next: OrderStatus): Promise<Order> {
    const order = await this.get(id);
    const allowed: Record<OrderStatus, OrderStatus[]> = {
      pending: ["paid", "cancelled"],
      paid: ["shipped", "cancelled"],
      shipped: [],
      cancelled: [],
    };
    if (!allowed[order.status].includes(next)) {
      throw new OrderError(`cannot go from ${order.status} to ${next}`, "BAD_STATE");
    }
    await this.db.exec("UPDATE orders SET status = $1 WHERE id = $2", [next, id]);
    order.status = next;
    this.emit(next, order);
    return order;
  }

  async cancelStale(olderThanMs: number): Promise<number> {
    const cutoff = new Date(Date.now() - olderThanMs);
    const stale = await this.db.many<Order>(
      "SELECT * FROM orders WHERE status = 'pending' AND created_at < $1",
      [cutoff],
    );
    let cancelled = 0;
    for (const order of stale) {
      try {
        await this.transition(order.id, "cancelled");
        cancelled++;
      } catch (err) {
        console.warn(`could not cancel ${order.id}`, err);
      }
    }
    return cancelled;
  }
}

export const summarize = (orders: Order[]): Record<OrderStatus, number> => {
  const counts: Record<OrderStatus, number> = { pending: 0, paid: 0, shipped: 0, cancelled: 0 };
  for (const o of orders) {
    counts[o.status] += 1;
  }
  return counts;
};

export function formatOrder(order: Order, svc: OrderService): string {
  const lines = order.items.map((i) => `${i.qty} x ${i.sku} @ ${i.unitPrice.format()}`);
  lines.unshift(`Order ${order.id} (${order.status})`);
  lines.push(`Total: ${svc.total(order).format()}`);
  return lines.join("\n");
}
