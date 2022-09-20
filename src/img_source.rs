use std::{fs, path::PathBuf};

use cosmic_bg_config::CosmicBgImgSource;
use notify::{
    event::{ModifyKind, RenameMode},
    RecommendedWatcher, RecursiveMode, Watcher,
};
use sctk::reexports::calloop::{
    channel::{self, channel},
    LoopHandle,
};

use crate::CosmicBg;

pub fn img_source(
    bg_sources: Vec<CosmicBgImgSource>,
    handle: LoopHandle<CosmicBg>,
) -> (
    channel::Sender<(CosmicBgImgSource, notify::Event)>,
    Vec<RecommendedWatcher>,
) {
    let sources: Vec<PathBuf> = bg_sources
        .iter()
        .cloned()
        .filter_map(|source| source.try_into().ok())
        .collect();
    let (notify_tx, notify_rx) = channel();
    let _ = handle
        .insert_source(
            notify_rx,
            |e: channel::Event<(CosmicBgImgSource, notify::Event)>, _, state| {
                match e {
                    channel::Event::Msg((source, event)) => match event.kind {
                        notify::EventKind::Create(_)
                        | notify::EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                            for w in state.wallpapers.iter_mut().filter(|w| w.source == source) {
                                for p in &event.paths {
                                    if !w.image_queue.contains(p) {
                                        w.image_queue.push_front(p.into());
                                    }
                                }
                                w.image_queue.retain(|p| !event.paths.contains(p));
                            }
                        }
                        notify::EventKind::Remove(_)
                        | notify::EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                            for w in state.wallpapers.iter_mut().filter(|w| w.source == source) {
                                w.image_queue.retain(|p| !event.paths.contains(p));
                            }
                        }
                        e => {
                            dbg!(e);
                        }
                    },
                    channel::Event::Closed => {
                        // TODO log drop
                    }
                }
            },
        )
        .map(|_| {})
        .map_err(|err| anyhow::anyhow!("{}", err));

    let notify_tx_clone = notify_tx.clone();
    (
        notify_tx,
        bg_sources
            .iter()
            .zip(sources)
            .filter_map(|(cosmic_source, path_source)| {
                let cosmic_source_clone = cosmic_source.clone();
                let tx_clone = notify_tx_clone.clone();
                let mut watcher = match RecommendedWatcher::new(
                    move |res| {
                        if let Ok(e) = res {
                            let _ = tx_clone.send((cosmic_source_clone.clone(), e));
                        }
                    },
                    notify::Config::default(),
                ) {
                    Ok(w) => w,
                    Err(_) => return None,
                };

                if let Ok(m) = fs::metadata(&path_source) {
                    if m.is_dir() {
                        let _ = watcher.watch(&path_source, RecursiveMode::Recursive);
                    } else if m.is_file() {
                        let _ = watcher.watch(&path_source, RecursiveMode::NonRecursive);
                    }
                }

                Some(watcher)
            })
            .collect(),
    )
}
