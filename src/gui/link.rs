//! Через что окно работает с диском: обычно — через фоновую часть (диск
//! живёт и без окна), в демо-режиме — напрямую.

use super::engine::{CreateRequest, Engine, Event, Notify, View};
use super::proto::ProcMem;
#[cfg(windows)]
use super::remote::Remote;

pub enum Link {
    Local(Engine),
    #[cfg(windows)]
    Remote(Remote),
}

impl Link {
    pub fn new(notify: Notify, demo: bool) -> Self {
        #[cfg(windows)]
        if !demo {
            return Link::Remote(Remote::new(notify));
        }
        let _ = demo;
        Link::Local(Engine::new(notify))
    }

    pub fn view(&self) -> View {
        match self {
            Link::Local(e) => e.view(),
            #[cfg(windows)]
            Link::Remote(r) => r.view(),
        }
    }

    pub fn events(&self) -> Vec<Event> {
        match self {
            Link::Local(e) => e.events(),
            #[cfg(windows)]
            Link::Remote(r) => r.events(),
        }
    }

    pub fn create(&self, req: CreateRequest) {
        match self {
            Link::Local(e) => e.create(req),
            #[cfg(windows)]
            Link::Remote(r) => r.create(req),
        }
    }

    pub fn cancel(&self) {
        match self {
            Link::Local(e) => e.cancel(),
            #[cfg(windows)]
            Link::Remote(r) => r.cancel(),
        }
    }

    pub fn remove(&self, force: bool) {
        match self {
            Link::Local(e) => e.remove(force),
            #[cfg(windows)]
            Link::Remote(r) => r.remove(force),
        }
    }

    /// Не сняли ли диск другой программой (фоновая часть следит за этим сама).
    pub fn tick(&self) {
        match self {
            Link::Local(e) => e.check_external(),
            #[cfg(windows)]
            Link::Remote(_) => {}
        }
    }

    /// Диск живёт в фоновой части: окно можно закрыть.
    pub fn background(&self) -> bool {
        !matches!(self, Link::Local(_))
    }

    /// Сколько памяти занимает фоновая часть.
    pub fn server_mem(&self) -> Option<ProcMem> {
        match self {
            Link::Local(_) => None,
            #[cfg(windows)]
            Link::Remote(r) => r.mem(),
        }
    }
}
