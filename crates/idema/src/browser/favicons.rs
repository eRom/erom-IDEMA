//! Les favicons déjà lus pendant la session, par origine : une page d'une origine connue montre son icône dès sa
//! navigation, sans refaire le téléchargement. En mémoire seulement, jamais dans `state.json`. Borné : les plus anciens
//! sortent, sauf ceux qu'un onglet montre encore. `I` est l'image décodée (`Arc<RenderImage>` en vrai, un entier dans les
//! tests).

use std::collections::{HashMap, VecDeque};

/// Combien d'origines on garde : quelques ko chacune, assez pour tous les sites d'une journée de travail.
const CAPACITY: usize = 64;

pub struct FaviconCache<I> {
    entries: HashMap<String, I>,
    /// Les origines, la plus ancienne en tête.
    order: VecDeque<String>,
}

impl<I> Default for FaviconCache<I> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<I: Clone> FaviconCache<I> {
    pub fn get(&self, origin: &str) -> Option<I> {
        self.entries.get(origin).cloned()
    }

    /// Garde l'icône de `origin`. Rend les images qui sortent du cache (l'ancienne de cette origine, ou les plus anciennes
    /// d'un cache plein) : l'appelant les rend à GPUI. `in_use` dit si un onglet montre encore une image : celle-là ne sort
    /// pas (le cache dépasse alors sa borne d'autant, au plus le nombre d'onglets), car la rendre à GPUI sous un onglet
    /// qui la peint la ferait disparaître.
    pub fn insert(&mut self, origin: String, image: I, in_use: impl Fn(&I) -> bool) -> Vec<I> {
        let mut gone = Vec::new();
        if let Some(old) = self.entries.insert(origin.clone(), image) {
            gone.push(old);
            self.order.retain(|known| *known != origin);
        }
        self.order.push_back(origin);
        while self.order.len() > CAPACITY {
            // La plus ancienne origine dont aucun onglet ne montre l'image ; la dernière entrée est l'image qui vient
            // d'arriver, jamais évincée.
            let evictable = self
                .order
                .iter()
                .take(self.order.len() - 1)
                .position(|origin| self.entries.get(origin).is_some_and(|image| !in_use(image)));
            let Some(index) = evictable else {
                break;
            };
            if let Some(origin) = self.order.remove(index)
                && let Some(old) = self.entries.remove(&origin)
            {
                gone.push(old);
            }
        }
        gone
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unused(_: &i32) -> bool {
        false
    }

    #[test]
    fn a_cached_icon_is_found_by_its_origin() {
        let mut cache = FaviconCache::default();
        assert!(cache.insert("https://a.test".into(), 1, unused).is_empty());
        assert_eq!(cache.get("https://a.test"), Some(1));
        assert_eq!(cache.get("https://b.test"), None);
    }

    #[test]
    fn a_new_icon_for_an_origin_returns_the_old_one_to_release() {
        let mut cache = FaviconCache::default();
        cache.insert("https://a.test".into(), 1, unused);
        assert_eq!(cache.insert("https://a.test".into(), 2, unused), vec![1]);
        assert_eq!(cache.get("https://a.test"), Some(2));
    }

    #[test]
    fn a_full_cache_drops_its_oldest_origin_first() {
        let mut cache = FaviconCache::default();
        for n in 0..CAPACITY as i32 {
            assert!(
                cache
                    .insert(format!("https://{n}.test"), n, unused)
                    .is_empty()
            );
        }
        // Une origine relue en dernier devient la plus récente : la deuxième plus ancienne sort ensuite.
        cache.insert("https://0.test".into(), 100, unused);
        assert_eq!(
            cache.insert("https://new.test".into(), 200, unused),
            vec![1]
        );
        assert_eq!(cache.get("https://1.test"), None);
        assert_eq!(cache.get("https://0.test"), Some(100));
        assert_eq!(cache.get("https://new.test"), Some(200));
    }

    #[test]
    fn an_icon_a_tab_still_shows_is_never_evicted() {
        let mut cache = FaviconCache::default();
        for n in 0..CAPACITY as i32 {
            cache.insert(format!("https://{n}.test"), n, unused);
        }
        // Un onglet montre encore les icônes 0 et 1 : la plus ancienne évincée est la 2.
        let shown = |image: &i32| *image == 0 || *image == 1;
        assert_eq!(cache.insert("https://new.test".into(), 200, shown), vec![2]);
        assert_eq!(cache.get("https://0.test"), Some(0));
        assert_eq!(cache.get("https://1.test"), Some(1));
        assert_eq!(cache.get("https://2.test"), None);
        // Tout est montré : rien ne sort, le cache dépasse sa borne plutôt que de retirer une image affichée.
        let all_shown = |_: &i32| true;
        assert!(
            cache
                .insert("https://newer.test".into(), 300, all_shown)
                .is_empty()
        );
        assert_eq!(cache.get("https://3.test"), Some(3));
    }
}
