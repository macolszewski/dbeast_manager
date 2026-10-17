drop database if exists shop;
create database shop;
use shop;
create table customers (id int auto_increment primary key, email varchar(200) not null unique, name varchar(100), bio text, avatar blob, created_at datetime default current_timestamp);
create table orders (id bigint auto_increment primary key, customer_id int not null, total decimal(12,2) not null, paid tinyint(1) default 0, placed_on date, score double,
  index orders_customer_idx (customer_id, placed_on), constraint fk_customer foreign key (customer_id) references customers(id));
create view customer_totals as select c.email, sum(o.total) as total from customers c join orders o on o.customer_id = c.id group by c.email;
insert into customers (email, name, bio, avatar) values ('a@example.com', 'Zażółć', 'line1\nline2', x'DEADBEEF'), ('b@example.com', null, null, null);
insert into orders (customer_id, total, paid, placed_on, score) values (1, 10.50, 1, '2026-10-01', 1.5), (1, 99.99, 0, null, null), (2, 0.01, 1, '2026-10-02', -2.25);
