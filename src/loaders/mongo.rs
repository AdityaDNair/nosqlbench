//! MongoDB loader: drop the database, `insert_many` batches of 10,000
//! (`ordered: false`), then build the indexes.

use anyhow::{bail, Result};
use mongodb::bson::{doc, DateTime, Document};
use mongodb::{Collection, IndexModel};

use super::Phase;
use crate::adapters::mongo::{connect, DB, META};
use crate::config::Config;
use crate::dataset::Dataset;

const BATCH: usize = 10_000;

async fn insert_all(
    coll: &Collection<Document>,
    docs: impl Iterator<Item = Document>,
) -> Result<u64> {
    let mut batch = Vec::with_capacity(BATCH);
    let mut n = 0u64;
    for d in docs {
        batch.push(d);
        if batch.len() == BATCH {
            n += batch.len() as u64;
            coll.insert_many(std::mem::take(&mut batch))
                .ordered(false)
                .await?;
            batch.reserve(BATCH);
        }
    }
    if !batch.is_empty() {
        n += batch.len() as u64;
        coll.insert_many(batch).ordered(false).await?;
    }
    Ok(n)
}

pub async fn load(cfg: &Config, ds: &Dataset) -> Result<()> {
    let db = connect(cfg).await?.database(DB);
    db.drop().await?;
    eprintln!("  dropped database {DB}");

    let users = db.collection::<Document>("users");
    let posts = db.collection::<Document>("posts");
    let follows = db.collection::<Document>("follows");

    let ph = Phase::start("users");
    let n = insert_all(
        &users,
        ds.users.iter().map(|u| {
            doc! {
                "_id": u.id as i64,
                "username": &u.username,
                "display_name": &u.display_name,
                "bio": &u.bio,
                "created_at": DateTime::from_millis(u.created_at_ms),
            }
        }),
    )
    .await?;
    ph.done(n);

    let ph = Phase::start("posts");
    let n = insert_all(
        &posts,
        ds.posts.iter().map(|p| {
            let tags: Vec<&str> = p
                .tags()
                .iter()
                .map(|t| ds.tag_names[*t as usize].as_str())
                .collect();
            doc! {
                "_id": p.id as i64,
                "author_id": p.author as i64,
                "body": ds.bodies.get(p.id as u32),
                "hashtags": tags,
                "like_count": 0i64,
                "created_at": DateTime::from_millis(p.created_at_ms),
            }
        }),
    )
    .await?;
    ph.done(n);

    let ph = Phase::start("follows");
    // Follows predate the post window; the exact time is not used by any op.
    let followed_at = DateTime::from_millis(crate::dataset::EPOCH_MS - 30 * crate::dataset::DAY_MS);
    let n = insert_all(
        &follows,
        (0..ds.users.len() as u64).flat_map(|u| {
            ds.following(u).iter().map(move |&t| {
                doc! {
                    "_id": { "f": u as i64, "t": t as i64 },
                    "follower_id": u as i64,
                    "followee_id": t as i64,
                    "followed_at": followed_at,
                }
            })
        }),
    )
    .await?;
    ph.done(n);

    let ph = Phase::start("indexes");
    posts
        .create_index(
            IndexModel::builder()
                .keys(doc! { "author_id": 1, "created_at": -1 })
                .build(),
        )
        .await?;
    posts
        .create_index(
            IndexModel::builder()
                .keys(doc! { "hashtags": 1, "created_at": -1 })
                .build(),
        )
        .await?;
    follows
        .create_index(
            IndexModel::builder()
                .keys(doc! { "follower_id": 1 })
                .build(),
        )
        .await?;
    follows
        .create_index(
            IndexModel::builder()
                .keys(doc! { "followee_id": 1 })
                .build(),
        )
        .await?;
    ph.done(4);

    // Sanity check against the generated dataset.
    let (nu, np, nf) = (
        users.count_documents(doc! {}).await?,
        posts.count_documents(doc! {}).await?,
        follows.count_documents(doc! {}).await?,
    );
    if nu != ds.users.len() as u64
        || np != ds.posts.len() as u64
        || nf != ds.follow_targets.len() as u64
    {
        bail!("loaded counts users={nu} posts={np} follows={nf} do not match the dataset");
    }
    db.collection::<Document>(META)
        .insert_one(doc! { "_id": "scenario", "value": cfg.workload.scenario.as_str() })
        .await?;
    eprintln!("  loaded: {nu} users, {np} posts, {nf} follows; 4 indexes");
    Ok(())
}
