-- Your SQL goes here
create table timers (
    id bigint primary key,
    at bigint not null,
    whom varchar not null,
    operation varchar not null
);

