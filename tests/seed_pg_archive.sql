-- Second database on the test server, for the database chooser.
create schema archive;
create table archive.invoices (id int primary key, total numeric(10, 2));
insert into archive.invoices values (1, 100.00), (2, 250.50);
