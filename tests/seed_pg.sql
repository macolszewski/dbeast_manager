drop schema if exists shop cascade;
create schema shop;
create type shop.status as enum ('new', 'paid', 'shipped');
create table shop.customers (
    id serial primary key,
    email text not null unique,
    name varchar(100),
    created_at timestamptz not null default now(),
    tags text[],
    meta jsonb
);
create table shop.orders (
    id bigserial primary key,
    customer_id int not null references shop.customers(id),
    total numeric(12, 2) not null,
    status shop.status not null default 'new',
    placed_on date,
    paid boolean default false,
    ref uuid,
    ip inet,
    duration interval
);
create index orders_customer_idx on shop.orders (customer_id, placed_on);
create view shop.customer_totals as
    select c.email, sum(o.total) as total from shop.customers c join shop.orders o on o.customer_id = c.id group by c.email;
create function shop.order_count(cid int) returns bigint as $$
    select count(*) from shop.orders where customer_id = cid;
$$ language sql;
insert into shop.customers (email, name, tags, meta)
select 'user' || g || '@example.com', 'Customer ' || g, array['a', 'b' || g], jsonb_build_object('n', g, 'vip', g % 7 = 0)
from generate_series(1, 1200) g;
insert into shop.orders (customer_id, total, status, placed_on, paid, ref, ip, duration)
select 1 + (g % 1200), (g * 13.37)::numeric(12,2), (array['new','paid','shipped'])[1 + g % 3]::shop.status,
       date '2026-01-01' + g % 280, g % 2 = 0, gen_random_uuid(), '10.0.0.1', make_interval(hours => g % 30, mins => g % 60)
from generate_series(1, 5000) g;
analyze;
