use fell_core::{AccountId, FolderSpan, SearchOptions, Store};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::hint::black_box;
use std::time::Instant;

const QUERIES: &[&str] = &[
    "in:inbox",
    "in:inbo",
    "in:Archive is:unread",
    "invoice",
    "boiler leak",
    "\"boarding pass\"",
    "warranty from:p10@bench.test in:inbox has:attachment after:2023-12-01",
    "invioce",
];

fn main() -> fell_core::Result<()> {
    let store = Store::open(std::env::args_os().nth(1).unwrap())?;
    let account = AccountId("bench".into());
    let options = SearchOptions { semantic: false };
    black_box(store.folder_page(&account, "INBOX", &FolderSpan::Top { rows: 100 })?);
    for &query in QUERIES {
        let result = store.search_with(&account, query, options)?;
        let effective = result.did_you_mean.as_deref().unwrap_or(query);
        let locations = store.search_locations(&account, effective, "INBOX", &result.hits)?;
        let mut digest = DefaultHasher::new();
        result.did_you_mean.hash(&mut digest);
        for hit in &result.hits {
            hit.message_id.hash(&mut digest);
            hit.thread_id.hash(&mut digest);
            hit.subject.hash(&mut digest);
            hit.snippet.hash(&mut digest);
            hit.sender_email.hash(&mut digest);
            hit.date_unix.hash(&mut digest);
            if let Some(location) = locations.get(&hit.message_id) {
                location.folder.hash(&mut digest);
                location.unread.hash(&mut digest);
                location.starred.hash(&mut digest);
            }
        }
        let mut query_samples = Vec::new();
        let mut prepared_samples = Vec::new();
        for _ in 0..31 {
            let start = Instant::now();
            let results = store.search_with(&account, query, options)?;
            let ranked = start.elapsed().as_secs_f64() * 1_000.;
            let effective = results.did_you_mean.as_deref().unwrap_or(query);
            black_box(store.search_locations(&account, effective, "INBOX", &results.hits)?);
            query_samples.push(ranked);
            prepared_samples.push(start.elapsed().as_secs_f64() * 1_000.);
        }
        query_samples.sort_by(f64::total_cmp);
        prepared_samples.sort_by(f64::total_cmp);
        println!(
            "{query}\t{}\t{:016x}\t{:.3}\t{:.3}\t{:.3}\t{:.3}",
            result.hits.len(), digest.finish(), query_samples[15], query_samples[29],
            prepared_samples[15], prepared_samples[29]
        );
    }
    Ok(())
}
