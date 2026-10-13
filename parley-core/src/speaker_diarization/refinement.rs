//! Post-session offline refinement.
//!
//! The live path clusters greedily: each embedding is compared against the
//! centroids that exist *at that moment* and joins the best one above
//! threshold. That decision is irreversible and order-dependent — an early
//! embedding can seed a cluster that later turns out to be two people, and
//! the same voice arriving before its cluster has a stable centroid can be
//! stranded under its own "Speaker N".
//!
//! Once the session is over (a live recording stops, or a batch import /
//! retranscription has embedded every segment) we have every embedding, so
//! we re-cluster offline in five steps:
//!
//! 1. **Average-linkage hierarchical agglomerative clustering** (HAC). Every
//!    pairwise distance is known up front, so the result depends only on the
//!    *set* of embeddings, not the order they arrived in. HAC can split a
//!    cluster the live pass wrongly merged and merge clusters it wrongly
//!    split. Speaker count is unknown, so the dendrogram is cut on a
//!    similarity threshold rather than a fixed k.
//!    With channel sources (see [`SegmentSource`]) each channel is clustered
//!    on its own: nobody is legitimately both in the user's room and on the
//!    far end of the call.
//! 2. **Same-voice merge.** Clusters on one channel whose *centroids* are
//!    at least [`RefineOptions::merge_similarity`] alike are merged
//!    (mic-channel pairs at the lower [`RefineOptions::mic_merge_similarity`]).
//!    Centroids average away per-segment noise that keeps average-linkage
//!    from joining two halves of one voice.
//! 3. **Minor-cluster folding** (#102). Real meetings left the HAC cut with
//!    a long tail of clusters holding a few seconds of speech each — a
//!    laugh, a one-word reply, crosstalk — that average-linkage can't
//!    attach to anyone because a single short embedding is noisy. A cluster
//!    with less than [`RefineOptions::min_cluster_secs`] of speech and no
//!    profile-matched members joins the nearest substantial cluster when its
//!    centroid is at least [`RefineOptions::fold_similarity`] alike. That's a
//!    much lower bar than the HAC cut, which is safe because it only ever
//!    moves a few seconds of speech and never merges two substantial voices.
//!    Clusters made of sub-second fragments, and strays on the mic channel,
//!    fold into the nearest voice on their channel whatever the score.
//!    With a known head count and fewer substantial voices than that, the
//!    largest minor clusters (one per unaccounted-for attendee) need
//!    [`RefineOptions::missing_attendee_fold_similarity`] instead: a quiet
//!    invitee shouldn't be folded into someone else.
//! 4. **Speaker cap.** When the caller knows roughly how many people to
//!    expect (calendar attendees — invitees skip meetings, others join
//!    late or uninvited), unnamed clusters keep merging closest-first
//!    until the *labels* fit that count — but never below
//!    [`RefineOptions::cap_similarity_floor`], set near the same-voice
//!    merge so the attendee list can't fuse genuinely different voices,
//!    and never two named people. An extra "Speaker N" is easier to fix
//!    than one label covering two people.
//! 5. **Naming**, below.
//!
//! ## Profile handling
//!
//! Segments the live pass matched to a stored profile ("pinned" rows) are
//! authoritative: their label and profile id are never changed here. They do
//! participate in the clustering, though, and lend their identity outward —
//! a cluster containing pinned "John" rows labels its *unpinned* members
//! "John" too. This repairs the common live-pass artifact where half a
//! voice's segments cross the profile-match threshold and the rest sit just
//! under it, splitting one person into "John" plus a parallel "Speaker N".
//!
//! Clusters with no pinned members get a second chance against the stored
//! profiles: the cluster centroid (averaged over the whole session, so much
//! less noisy than any single segment) is searched against the matcher.
//! Only clusters that still match nothing fall back to "Speaker N",
//! numbered by first appearance so the first unrecognized voice heard is
//! "Speaker 1".
//!
//! ## The local user ("self")
//!
//! When segments carry a [`SegmentSource`], the mic channel is treated as
//! authoritative for the local user:
//! - a *system*-channel segment that matched the self profile only weakly
//!   (below [`RefineOptions::self_strong_match`]) is not pinned to it — the
//!   far end of a call can't be the user unless the voice match is strong —
//!   and a system-dominated cluster is named after the user only on a strong
//!   centroid match;
//! - a *mic*-dominated cluster joins the self profile at the looser
//!   [`RefineOptions::mic_self_similarity`], so the user's own voice doesn't
//!   splinter into parallel "Speaker N" clusters when individual segments
//!   miss the strict profile threshold.
//!
//! A profile with the same name as the self profile counts as the user too
//! (see [`SpeakerProfileMatcher::is_self`]), scored against the real self
//! profile.

use crate::speaker_diarization::embedding_math::{cosine_distance, dot, l2_normalize};
use crate::speaker_diarization::{EmbeddingRecord, SegmentSource, SpeakerProfileMatcher};
use serde::Serialize;
use std::collections::HashMap;

/// Tuning for [`refine`]. [`RefineOptions::default`] is what the app uses;
/// the values come from the measurements on real recordings in #102 (see
/// `measure.rs`).
#[derive(Debug, Clone)]
pub struct RefineOptions {
    /// Cosine-similarity cut for the HAC dendrogram (merge while the
    /// closest pair of clusters is at least this alike).
    pub threshold: f32,
    /// After HAC, clusters on the same channel whose *centroids* are at
    /// least this alike are merged. Centroids average away the per-segment
    /// noise that keeps average-linkage from joining two halves of one voice
    /// (in #102's data, stored profiles of different people sit at 0.05–0.30
    /// centroid similarity; one person's two profiles at 0.57).
    pub merge_similarity: f32,
    /// [`Self::merge_similarity`] for two mic-channel clusters. Lower,
    /// because the mic is the user's own channel: one person's voice there
    /// drifts with distance to the mic and gain far more than a remote
    /// codec stream does, while an in-room second voice is the exception.
    pub mic_merge_similarity: f32,
    /// Clusters holding less speech than this (seconds) are "minor" and get
    /// folded into the nearest substantial cluster. `0.0` disables folding.
    pub min_cluster_secs: f32,
    /// Minimum centroid similarity for a minor cluster to fold.
    pub fold_similarity: f32,
    /// [`Self::fold_similarity`] for a minor cluster that may be an invitee
    /// who barely spoke: with a known head count ([`Self::max_speakers`])
    /// and fewer substantial voices than that, the largest minor clusters
    /// (one per missing attendee) must be this alike to be folded.
    pub missing_attendee_fold_similarity: f32,
    /// Rough number of people expected, e.g. the calendar attendee count.
    /// `None` = unknown. Only a guide: invitees skip meetings, others join
    /// uninvited or late. Above it, clusters merge down toward it (see
    /// [`Self::cap_similarity_floor`]); below it, minor clusters are kept
    /// apart more readily (see [`Self::missing_attendee_fold_similarity`]).
    pub max_speakers: Option<usize>,
    /// Never merge two clusters less alike than this to satisfy
    /// `max_speakers`. Close to [`Self::merge_similarity`]: the attendee
    /// list is a hint, and merging two different people to match it (0.42
    /// alike in #102's data) is worse than one extra "Speaker N".
    pub cap_similarity_floor: f32,
    /// Honour [`SegmentSource`] for self-attribution (see module docs).
    pub use_sources: bool,
    /// Score a system-channel segment or cluster needs against the self
    /// profile to be labelled as the user.
    pub self_strong_match: f32,
    /// Centroid similarity at which a mic-dominated cluster is labelled as
    /// the user.
    pub mic_self_similarity: f32,
    /// Minor clusters whose segments average shorter than this (seconds)
    /// fold into the nearest same-channel cluster regardless of
    /// `fold_similarity` — CAM++ embeddings of sub-second audio are too
    /// noisy to establish a voice of their own.
    pub short_segment_secs: f32,
}

impl Default for RefineOptions {
    fn default() -> Self {
        Self {
            threshold: crate::speaker_diarization::REFINE_CLUSTER_THRESHOLD,
            merge_similarity: 0.60,
            mic_merge_similarity: 0.40,
            min_cluster_secs: 20.0,
            fold_similarity: 0.30,
            missing_attendee_fold_similarity: 0.45,
            max_speakers: None,
            cap_similarity_floor: 0.55,
            use_sources: true,
            self_strong_match: 0.70,
            mic_self_similarity: 0.45,
            short_segment_secs: 1.5,
        }
    }
}

impl RefineOptions {
    /// The refinement as it was before #102: HAC cut only, no folding, no
    /// speaker cap, sources ignored. Kept for the measurement harness.
    pub fn legacy() -> Self {
        Self {
            threshold: crate::speaker_diarization::REFINE_CLUSTER_THRESHOLD,
            merge_similarity: 1.1,
            mic_merge_similarity: 1.1,
            min_cluster_secs: 0.0,
            fold_similarity: 1.1,
            missing_attendee_fold_similarity: 1.1,
            max_speakers: None,
            cap_similarity_floor: 1.1,
            use_sources: false,
            self_strong_match: 0.0,
            mic_self_similarity: 1.1,
            short_segment_secs: 0.0,
        }
    }

    /// Default tuning with a known head count as the speaker cap.
    pub fn with_max_speakers(mut self, max_speakers: Option<usize>) -> Self {
        self.max_speakers = max_speakers.filter(|&n| n > 0);
        self
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct RefinedAssignment {
    pub sequence_id: u64,
    pub speaker: String,
    /// Profile id after refinement. `Some` for a segment pinned to a profile
    /// by the live pass (we only ever unpin a weak self match on the system
    /// channel), and may be newly `Some` for a segment whose cluster was
    /// folded into a stored profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_profile_id: Option<String>,
    /// True when this segment's label or profile link changed vs the live
    /// pass — the frontend can use this to flash-update only the affected
    /// rows, and the DB writer uses it to skip no-op updates.
    pub changed: bool,
    /// The label the live pass assigned. The DB update matches on this so a
    /// row the user renamed underneath us is never clobbered.
    #[serde(skip)]
    pub previous_speaker: String,
}

/// One working cluster: its member records plus running sums.
#[derive(Debug, Clone)]
struct Cluster {
    members: Vec<usize>,
    /// Sum of member embeddings (unnormalized); normalize for a centroid.
    sum: Vec<f32>,
    secs: f32,
    mic_secs: f32,
    system_secs: f32,
    /// Distinct pinned profile ids among the members.
    pinned: Vec<String>,
    /// The channel group HAC formed this cluster in (always `Unknown` when
    /// sources are ignored).
    group: SegmentSource,
}

impl Cluster {
    fn centroid(&self) -> Vec<f32> {
        let mut c = self.sum.clone();
        l2_normalize(&mut c);
        c
    }

    fn absorb(&mut self, other: Cluster) {
        self.members.extend(other.members);
        self.members.sort_unstable();
        for (a, b) in self.sum.iter_mut().zip(&other.sum) {
            *a += b;
        }
        self.secs += other.secs;
        self.mic_secs += other.mic_secs;
        self.system_secs += other.system_secs;
        if self.group == SegmentSource::Unknown {
            self.group = other.group;
        }
        for p in other.pinned {
            if !self.pinned.contains(&p) {
                self.pinned.push(p);
            }
        }
    }

    /// More system-channel than mic-channel speech.
    fn system_dominated(&self) -> bool {
        self.system_secs > self.mic_secs
    }

    /// Mostly mic-channel speech.
    fn mic_dominated(&self) -> bool {
        self.mic_secs > 0.0 && self.mic_secs >= self.secs * SegmentSource::DOMINANCE as f32
    }

    /// Whether this cluster may be merged into `other`: never across
    /// channels (an unclear-channel cluster may join either), and never
    /// between two different pinned profiles.
    fn may_join(&self, other: &Cluster) -> bool {
        let same_channel = self.group == other.group
            || self.group == SegmentSource::Unknown
            || other.group == SegmentSource::Unknown;
        let same_person = self.pinned.is_empty()
            || other.pinned.is_empty()
            || self.pinned.iter().any(|p| other.pinned.contains(p));
        same_channel && same_person
    }
}

/// Re-cluster `history` offline and return one assignment per record, in
/// `history` order. `matcher` provides the stored voice profiles used to
/// name clusters (see module docs); `None` disables the profile steps and
/// everything unpinned falls back to "Speaker N".
pub fn refine(
    history: &[EmbeddingRecord],
    options: &RefineOptions,
    matcher: Option<&SpeakerProfileMatcher>,
) -> Vec<RefinedAssignment> {
    if history.is_empty() {
        return Vec::new();
    }
    let is_self = |pid: &str| matcher.is_some_and(|m| m.is_self(pid));

    // Effective pins: the live pass's profile matches, minus weak self
    // matches heard on the system channel.
    let pins: Vec<Option<&str>> = history
        .iter()
        .map(|r| {
            let pid = r.voice_profile_id.as_deref()?;
            // Scored against the real self profile, not whichever same-named
            // duplicate matched — a duplicate may have absorbed other voices.
            let self_score = matcher
                .and_then(|m| m.self_match(&r.embedding))
                .map(|m| m.score)
                .or(r.profile_score);
            let weak_self_on_system = options.use_sources
                && is_self(pid)
                && r.source == SegmentSource::System
                && self_score.is_some_and(|s| s < options.self_strong_match);
            (!weak_self_on_system).then_some(pid)
        })
        .collect();

    // 1. HAC over every record — pinned rows included, so their cluster
    //    mates can inherit the profile (pinned rows themselves never change).
    //    With channel sources, each channel is clustered on its own: the
    //    echo-cancelled mic only carries voices in the user's room and the
    //    system channel only the far end, so nobody legitimately appears on
    //    both — and letting HAC bridge them is exactly how the user's voice
    //    got smeared across the far end's clusters.
    let group_of = |r: &EmbeddingRecord| {
        if options.use_sources {
            r.source
        } else {
            SegmentSource::Unknown
        }
    };
    let mut cluster_of = vec![0usize; history.len()];
    let mut group_of_cluster: Vec<SegmentSource> = Vec::new();
    for group in [
        SegmentSource::Microphone,
        SegmentSource::System,
        SegmentSource::Unknown,
    ] {
        let indices: Vec<usize> = (0..history.len())
            .filter(|&i| group_of(&history[i]) == group)
            .collect();
        if indices.is_empty() {
            continue;
        }
        let embeddings: Vec<Vec<f32>> = indices
            .iter()
            .map(|&i| history[i].embedding.clone())
            .collect();
        let local = agglomerative_cluster(&embeddings, 1.0 - options.threshold);
        let offset = group_of_cluster.len();
        let count = local.iter().copied().max().map_or(0, |m| m + 1);
        for (k, &i) in indices.iter().enumerate() {
            cluster_of[i] = offset + local[k];
        }
        group_of_cluster.extend(std::iter::repeat(group).take(count));
    }
    let mut clusters = build_clusters(history, &pins, &cluster_of, &group_of_cluster);

    // 2. Merge clusters whose centroids say they're the same voice.
    merge_closest(
        &mut clusters,
        options.merge_similarity,
        options.mic_merge_similarity,
    );

    // 3. Fold minor clusters into the nearest substantial one.
    fold_minor_clusters(&mut clusters, options);

    // 4. Respect a known head count. Count *names*, not clusters: two
    //    clusters that both resolve to "John" are one speaker.
    if let Some(max) = options.max_speakers {
        let name_of = |c: &Cluster| {
            cluster_identity(c, history, &pins, matcher, options).map(|(name, _)| name)
        };
        cap_speakers(&mut clusters, max, options.cap_similarity_floor, &name_of);
    }

    // 5. Name clusters in first-appearance order so "Speaker N" numbering is
    //    stable and human-friendly: the first unrecognized voice to speak is
    //    "Speaker 1". Cluster indices carry no meaningful order.
    let mut cluster_index = vec![0usize; history.len()];
    for (c, cluster) in clusters.iter().enumerate() {
        for &m in &cluster.members {
            cluster_index[m] = c;
        }
    }
    let mut identity_of: HashMap<usize, (String, Option<String>)> = HashMap::new();
    let mut next_label = 1usize;
    for &c in &cluster_index {
        if identity_of.contains_key(&c) {
            continue;
        }
        let identity = cluster_identity(&clusters[c], history, &pins, matcher, options)
            .unwrap_or_else(|| {
                let label = format!("Speaker {}", next_label);
                next_label += 1;
                (label, None)
            });
        identity_of.insert(c, identity);
    }

    history
        .iter()
        .enumerate()
        .map(|(i, record)| {
            let (speaker, voice_profile_id) = match pins[i] {
                // Pinned: a live profile match is authoritative.
                Some(pid) => (record.label.clone(), Some(pid.to_string())),
                None => identity_of[&cluster_index[i]].clone(),
            };
            RefinedAssignment {
                sequence_id: record.sequence_id,
                changed: speaker != record.label || voice_profile_id != record.voice_profile_id,
                speaker,
                voice_profile_id,
                previous_speaker: record.label.clone(),
            }
        })
        .collect()
}

fn build_clusters(
    history: &[EmbeddingRecord],
    pins: &[Option<&str>],
    cluster_of: &[usize],
    group_of_cluster: &[SegmentSource],
) -> Vec<Cluster> {
    let dim = history[0].embedding.len();
    let mut clusters: Vec<Cluster> = group_of_cluster
        .iter()
        .map(|&group| Cluster {
            members: Vec::new(),
            sum: vec![0.0; dim],
            secs: 0.0,
            mic_secs: 0.0,
            system_secs: 0.0,
            pinned: Vec::new(),
            group,
        })
        .collect();
    for (i, record) in history.iter().enumerate() {
        let cluster = &mut clusters[cluster_of[i]];
        cluster.members.push(i);
        let mut e = record.embedding.clone();
        l2_normalize(&mut e);
        if e.len() == dim {
            for (a, b) in cluster.sum.iter_mut().zip(&e) {
                *a += b;
            }
        }
        let secs = record.duration_secs.max(0.0);
        cluster.secs += secs;
        match record.source {
            SegmentSource::Microphone => cluster.mic_secs += secs,
            SegmentSource::System => cluster.system_secs += secs,
            SegmentSource::Unknown => {}
        }
        if let Some(pid) = pins[i] {
            if !cluster.pinned.iter().any(|p| p == pid) {
                cluster.pinned.push(pid.to_string());
            }
        }
    }
    clusters.retain(|c| !c.members.is_empty());
    clusters
}

/// Step 2: every minor cluster joins the most similar substantial cluster if
/// the centroids are at least `fold_similarity` alike. Substantial centroids
/// are taken once up front, so the result doesn't depend on fold order.
///
/// A cluster is minor when it has no pinned members and either holds less
/// than `min_cluster_secs` of speech, or is made of segments with no clear
/// channel (crosstalk / both sides at once) in a recording that does have
/// channel information — those embeddings are a blend of two voices, so they
/// shouldn't stand as a speaker of their own. A minor cluster from one
/// channel only folds into a cluster from the same channel; an unclear one
/// may fold into either.
fn fold_minor_clusters(clusters: &mut Vec<Cluster>, options: &RefineOptions) {
    if options.min_cluster_secs <= 0.0 {
        return;
    }
    let has_channels = clusters.iter().any(|c| c.group != SegmentSource::Unknown);
    let is_minor = |c: &Cluster| {
        c.pinned.is_empty()
            && (c.secs < options.min_cluster_secs
                || (has_channels && c.group == SegmentSource::Unknown))
    };
    let majors: Vec<(usize, Vec<f32>)> = clusters
        .iter()
        .enumerate()
        .filter(|(_, c)| !is_minor(c))
        .map(|(i, c)| (i, c.centroid()))
        .collect();
    if majors.is_empty() {
        return;
    }

    // Fewer substantial voices than expected attendees: the largest minor
    // clusters (one per missing attendee) may be someone who barely spoke,
    // so they need a closer match to be folded. Fragments too short to
    // embed and mic-channel strays are never candidates.
    let too_short = |c: &Cluster| c.secs < options.short_segment_secs * c.members.len() as f32;
    let missing = options
        .max_speakers
        .map_or(0, |n| n.saturating_sub(majors.len()));
    let mut candidates: Vec<usize> = (0..clusters.len())
        .filter(|&i| {
            let c = &clusters[i];
            is_minor(c) && !too_short(c) && c.group != SegmentSource::Microphone
        })
        .collect();
    candidates.sort_by(|&a, &b| clusters[b].secs.total_cmp(&clusters[a].secs));
    candidates.truncate(missing);

    let mut target: Vec<Option<usize>> = vec![None; clusters.len()];
    for (i, cluster) in clusters.iter().enumerate() {
        if !is_minor(cluster) {
            continue;
        }
        let centroid = cluster.centroid();
        let best = majors
            .iter()
            .filter(|(j, _)| cluster.may_join(&clusters[*j]))
            .map(|(j, c)| (*j, dot(&centroid, c)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((j, sim)) = best {
            let floor = if too_short(cluster) || cluster.group == SegmentSource::Microphone {
                // Fragments too short for CAM++ to embed reliably have
                // similarity scores that are noise; and a few seconds on the
                // mic channel — the user's own channel — shouldn't stand as
                // a new speaker either. Both go to the nearest voice on
                // their channel whatever the score.
                f32::NEG_INFINITY
            } else if candidates.contains(&i) {
                options.missing_attendee_fold_similarity
            } else {
                options.fold_similarity
            };
            if sim >= floor {
                target[i] = Some(j);
            }
        }
    }

    let mut taken: Vec<Option<Cluster>> = clusters.drain(..).map(Some).collect();
    for i in 0..taken.len() {
        if let Some(j) = target[i] {
            let minor = taken[i].take().expect("minor clusters are folded once");
            taken[j]
                .as_mut()
                .expect("fold targets are substantial and never folded")
                .absorb(minor);
        }
    }
    clusters.extend(taken.into_iter().flatten());
}

/// Step 2: merge the most similar mergeable pair of clusters (by centroid),
/// again and again, until no pair clears its floor: `mic_floor` for two
/// mic-channel clusters, `floor` otherwise.
fn merge_closest(clusters: &mut Vec<Cluster>, floor: f32, mic_floor: f32) {
    let mut centroids: Vec<Vec<f32>> = clusters.iter().map(Cluster::centroid).collect();
    while clusters.len() > 1 {
        // (i, j, similarity minus the pair's floor)
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                if !clusters[i].may_join(&clusters[j]) {
                    continue;
                }
                let both_mic = clusters[i].group == SegmentSource::Microphone
                    && clusters[j].group == SegmentSource::Microphone;
                let pair_floor = if both_mic { mic_floor } else { floor };
                let margin = dot(&centroids[i], &centroids[j]) - pair_floor;
                if best.map_or(true, |(_, _, b)| margin > b) {
                    best = Some((i, j, margin));
                }
            }
        }
        match best {
            Some((i, j, margin)) if margin >= 0.0 => {
                let other = clusters.remove(j);
                centroids.remove(j);
                clusters[i].absorb(other);
                centroids[i] = clusters[i].centroid();
            }
            _ => break,
        }
    }
}

/// Step 4: while the clusters would be labelled as more than `max` distinct
/// speakers, merge the most similar pair in which at least one side is an
/// unrecognized voice (two named people are never fused, and two clusters
/// with the same name are one speaker already). Stops early once the best
/// candidate pair is less alike than `floor`.
///
/// `name_of` is the stored-profile name a cluster would be labelled with (see
/// [`cluster_identity`]), so the count matches the labels the user will see.
fn cap_speakers(
    clusters: &mut Vec<Cluster>,
    max: usize,
    floor: f32,
    name_of: &dyn Fn(&Cluster) -> Option<String>,
) {
    let mut names: Vec<Option<String>> = clusters.iter().map(name_of).collect();
    let mut centroids: Vec<Vec<f32>> = clusters.iter().map(Cluster::centroid).collect();
    loop {
        let mut named: Vec<&str> = names.iter().flatten().map(String::as_str).collect();
        named.sort_unstable();
        named.dedup();
        let speakers = named.len() + names.iter().filter(|n| n.is_none()).count();
        if speakers <= max.max(1) {
            break;
        }

        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..clusters.len() {
            for j in (i + 1)..clusters.len() {
                if (names[i].is_some() && names[j].is_some()) || !clusters[i].may_join(&clusters[j])
                {
                    continue;
                }
                let sim = dot(&centroids[i], &centroids[j]);
                if best.map_or(true, |(_, _, b)| sim > b) {
                    best = Some((i, j, sim));
                }
            }
        }
        match best {
            Some((i, j, sim)) if sim >= floor => {
                let other = clusters.remove(j);
                centroids.remove(j);
                names.remove(j);
                clusters[i].absorb(other);
                centroids[i] = clusters[i].centroid();
                names[i] = name_of(&clusters[i]);
            }
            _ => break,
        }
    }
}

/// A cluster's (label, profile id) when it can be tied to a stored profile.
/// `None` means "unrecognized voice" (caller assigns a "Speaker N").
fn cluster_identity(
    cluster: &Cluster,
    history: &[EmbeddingRecord],
    pins: &[Option<&str>],
    matcher: Option<&SpeakerProfileMatcher>,
    options: &RefineOptions,
) -> Option<(String, Option<String>)> {
    let is_self = |pid: &str| matcher.is_some_and(|m| m.is_self(pid));
    let centroid = cluster.centroid();
    // A system-dominated cluster is the user only on a strong voice match.
    let self_allowed = !options.use_sources
        || !cluster.system_dominated()
        || matcher
            .and_then(|m| m.self_match(&centroid))
            .is_some_and(|m| m.score >= options.self_strong_match);

    // Dominant pinned profile. Iterating in history order with a
    // strictly-greater comparison makes ties resolve to the profile pinned
    // first, deterministically.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut best: Option<(usize, usize)> = None; // (record index, count)
    for &i in &cluster.members {
        let Some(pid) = pins[i] else { continue };
        if !self_allowed && is_self(pid) {
            continue;
        }
        let count = counts.entry(pid).or_insert(0);
        *count += 1;
        if best.map_or(true, |(_, c)| *count > c) {
            best = Some((i, *count));
        }
    }
    if let Some((i, _)) = best {
        return Some((history[i].label.clone(), pins[i].map(str::to_string)));
    }

    // No pinned members — try the (low-noise) cluster centroid against the
    // stored profiles.
    let matcher = matcher?;
    if let Some(m) = matcher.search(&centroid) {
        if self_allowed || !matcher.is_self(&m.profile_id) {
            return Some((m.name, Some(m.profile_id)));
        }
    }

    // The mic channel is the user's: a mic-dominated cluster that sounds
    // reasonably like them is them.
    if options.use_sources && cluster.mic_dominated() {
        if let Some(m) = matcher.self_match(&centroid) {
            if m.score >= options.mic_self_similarity {
                return Some((m.name, Some(m.profile_id)));
            }
        }
    }
    None
}

/// Average-linkage HAC over `embeddings` using cosine distance, cutting the
/// dendrogram once the nearest pair exceeds `max_distance`.
///
/// Returns a cluster index per input (indices are arbitrary but consistent;
/// the caller relabels them).
///
/// Cost: O(n²) memory for the distance matrix. Merging keeps a cached
/// nearest-neighbour per active cluster, so the usual case is ~O(n²) total
/// rather than the O(n³) a naive full rescan per merge would cost.
fn agglomerative_cluster(embeddings: &[Vec<f32>], max_distance: f32) -> Vec<usize> {
    let n = embeddings.len();
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![0];
    }

    // Work on L2-normalized copies so cosine similarity is a plain dot product.
    let normed: Vec<Vec<f32>> = embeddings
        .iter()
        .map(|e| {
            let mut v = e.clone();
            l2_normalize(&mut v);
            v
        })
        .collect();

    // dist[i][j] — cosine distance. Only entries between *active* clusters
    // are meaningful; merged-away rows are ignored via `active`.
    let mut dist = vec![vec![0.0f32; n]; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let d = cosine_distance(&normed[i], &normed[j]);
            dist[i][j] = d;
            dist[j][i] = d;
        }
    }

    let mut active: Vec<bool> = vec![true; n];
    let mut size: Vec<f32> = vec![1.0; n];
    // Which original points live in each active cluster.
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();

    // nn[i] = (nearest active j, distance) for active i. Recomputed lazily
    // when a merge invalidates it.
    let mut nn: Vec<Option<(usize, f32)>> = (0..n)
        .map(|i| nearest_neighbour(i, &dist, &active))
        .collect();

    loop {
        // Global closest pair, via the cached per-cluster nearest neighbours.
        let mut best: Option<(usize, usize, f32)> = None;
        for i in 0..n {
            if !active[i] {
                continue;
            }
            if let Some((j, d)) = nn[i] {
                if best.map_or(true, |(_, _, bd)| d < bd) {
                    best = Some((i, j, d));
                }
            }
        }

        let Some((i, j, d)) = best else { break };
        // Dendrogram cut: nothing left that's close enough to merge.
        if d >= max_distance {
            break;
        }

        // Merge j into i using the Lance-Williams update for average linkage:
        //   d(k, i∪j) = (|i|·d(k,i) + |j|·d(k,j)) / (|i| + |j|)
        let (si, sj) = (size[i], size[j]);
        let total = si + sj;
        for k in 0..n {
            if !active[k] || k == i || k == j {
                continue;
            }
            let merged = (si * dist[i][k] + sj * dist[j][k]) / total;
            dist[i][k] = merged;
            dist[k][i] = merged;
        }
        size[i] = total;
        let moved = std::mem::take(&mut members[j]);
        members[i].extend(moved);
        active[j] = false;
        nn[j] = None;

        // i's distances all changed, and anyone pointing at i or j is stale.
        for k in 0..n {
            if !active[k] {
                continue;
            }
            if k == i || matches!(nn[k], Some((t, _)) if t == i || t == j) {
                nn[k] = nearest_neighbour(k, &dist, &active);
            }
        }
    }

    let mut out = vec![0usize; n];
    let mut cluster_index = 0usize;
    for i in 0..n {
        if !active[i] {
            continue;
        }
        for &m in &members[i] {
            out[m] = cluster_index;
        }
        cluster_index += 1;
    }
    out
}

/// Nearest active cluster to `i` (excluding itself), or `None` if `i` is the
/// only active cluster left.
fn nearest_neighbour(i: usize, dist: &[Vec<f32>], active: &[bool]) -> Option<(usize, f32)> {
    if !active[i] {
        return None;
    }
    let mut best: Option<(usize, f32)> = None;
    for (j, &is_active) in active.iter().enumerate() {
        if !is_active || j == i {
            continue;
        }
        let d = dist[i][j];
        if best.map_or(true, |(_, bd)| d < bd) {
            best = Some((j, d));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speaker_diarization::EmbeddingRecord;

    fn rec(seq: u64, emb: Vec<f32>, label: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            sequence_id: seq,
            embedding: emb,
            voice_profile_id: None,
            label: label.to_string(),
            profile_score: None,
            // Long enough that no cluster in these tests is "minor".
            duration_secs: 30.0,
            source: SegmentSource::Unknown,
        }
    }

    fn pinned(seq: u64, emb: Vec<f32>, label: &str, profile_id: &str) -> EmbeddingRecord {
        EmbeddingRecord {
            sequence_id: seq,
            embedding: emb,
            voice_profile_id: Some(profile_id.to_string()),
            label: label.to_string(),
            profile_score: Some(0.9),
            duration_secs: 30.0,
            source: SegmentSource::Unknown,
        }
    }

    /// Number of distinct speaker labels in a refinement result.
    fn distinct_speakers(out: &[RefinedAssignment]) -> usize {
        out.iter()
            .map(|a| a.speaker.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len()
    }

    #[test]
    fn empty_history_returns_empty() {
        let out = refine(&[], &RefineOptions::default(), None);
        assert!(out.is_empty());
    }

    #[test]
    fn profile_matched_rows_are_pinned() {
        let r = pinned(1, vec![1.0, 0.0], "John", "profile-1");
        let out = refine(&[r], &RefineOptions::default(), None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        assert!(!out[0].changed);
    }

    /// The profile-extension path: an unpinned row that clusters with pinned
    /// "John" rows inherits the profile instead of surfacing as a parallel
    /// "Speaker N" — while the pinned row itself never changes.
    #[test]
    fn cluster_mates_of_a_pinned_row_inherit_its_profile() {
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            pinned(2, vec![1.0, 0.0, 0.0], "John", "profile-1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        // The unpinned same-voice row folds into John, flagged as changed.
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        assert!(out[0].changed);
        // The pinned row is untouched.
        assert_eq!(out[1].speaker, "John");
        assert!(!out[1].changed);
        // The remaining unrecognized voice is the first unnamed cluster.
        assert_eq!(out[2].speaker, "Speaker 1");
        assert!(out[2].voice_profile_id.is_none());
    }

    /// Two pinned rows of *different* profiles keep their own labels even if
    /// HAC puts them in one cluster — pins are always authoritative.
    #[test]
    fn pinned_rows_never_change_even_when_clustered_together() {
        let history = vec![
            pinned(1, vec![1.0, 0.0, 0.0], "John", "profile-1"),
            pinned(2, vec![0.98, 0.02, 0.0], "Mary", "profile-2"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[1].speaker, "Mary");
        assert!(!out[0].changed);
        assert!(!out[1].changed);
    }

    /// Unpinned members of a mixed cluster follow the *dominant* pinned
    /// profile, with ties broken by first appearance.
    #[test]
    fn dominant_pinned_profile_wins_a_mixed_cluster() {
        let voice = vec![1.0, 0.0, 0.0];
        let history = vec![
            rec(1, voice.clone(), "Speaker 1"),
            pinned(2, voice.clone(), "Mary", "profile-2"),
            pinned(3, voice.clone(), "John", "profile-1"),
            pinned(4, voice.clone(), "John", "profile-1"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        // Pins keep their own labels regardless.
        assert_eq!(out[1].speaker, "Mary");
        assert_eq!(out[2].speaker, "John");
    }

    /// A cluster with no pinned members is searched against the stored
    /// profiles by centroid, so a voice whose individual segments all sat
    /// just under the live match threshold still gets named.
    #[test]
    fn unpinned_cluster_matches_profile_by_centroid() {
        let matcher = match SpeakerProfileMatcher::new(
            3,
            vec![(
                "profile-1".to_string(),
                "John".to_string(),
                vec![1.0, 0.0, 0.0],
            )],
            0.60,
        ) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skipping: SpeakerEmbeddingManager unavailable ({})", e);
                return;
            }
        };
        let history = vec![
            rec(1, vec![0.99, 0.01, 0.0], "Speaker 1"),
            rec(2, vec![0.98, 0.02, 0.0], "Speaker 1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, &RefineOptions::default(), Some(&matcher));
        assert_eq!(out[0].speaker, "John");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("profile-1"));
        assert!(out[0].changed);
        assert_eq!(out[1].speaker, "John");
        // The orthogonal voice matches nothing and stays unnamed.
        assert_eq!(out[2].speaker, "Speaker 1");
        assert!(out[2].voice_profile_id.is_none());
    }

    #[test]
    fn two_clear_groups_produce_two_clusters() {
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(2, vec![0.98, 0.02, 0.0], "Speaker 1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
            rec(4, vec![0.02, 0.98, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 2);
        assert_eq!(out[0].speaker, out[1].speaker);
        assert_eq!(out[2].speaker, out[3].speaker);
        assert_ne!(out[0].speaker, out[2].speaker);
    }

    #[test]
    fn a_third_distinct_voice_produces_three_clusters() {
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(2, vec![0.98, 0.02, 0.0], "Speaker 1"),
            rec(3, vec![0.0, 1.0, 0.0], "Speaker 2"),
            rec(4, vec![0.02, 0.98, 0.0], "Speaker 2"),
            rec(5, vec![0.0, 0.0, 1.0], "Speaker 3"),
            rec(6, vec![0.0, 0.02, 0.98], "Speaker 3"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 3);
        assert_eq!(out[4].speaker, out[5].speaker);
        assert_ne!(out[4].speaker, out[0].speaker);
        assert_ne!(out[4].speaker, out[2].speaker);
    }

    /// The whole point of offline refinement: the grouping must depend on the
    /// *set* of embeddings, not the order they arrived in.
    #[test]
    fn clustering_is_order_independent() {
        let a1 = vec![1.0, 0.0, 0.0];
        let a2 = vec![0.97, 0.03, 0.0];
        let b1 = vec![0.0, 1.0, 0.0];
        let b2 = vec![0.03, 0.97, 0.0];

        // Same four embeddings, two different arrival orders.
        let forward = vec![
            rec(1, a1.clone(), "Speaker 1"),
            rec(2, a2.clone(), "Speaker 1"),
            rec(3, b1.clone(), "Speaker 2"),
            rec(4, b2.clone(), "Speaker 2"),
        ];
        let interleaved = vec![
            rec(1, b1.clone(), "Speaker 1"),
            rec(2, a1.clone(), "Speaker 2"),
            rec(3, b2.clone(), "Speaker 1"),
            rec(4, a2.clone(), "Speaker 2"),
        ];

        let out_f = refine(&forward, &RefineOptions::default(), None);
        let out_i = refine(&interleaved, &RefineOptions::default(), None);

        assert_eq!(distinct_speakers(&out_f), 2);
        assert_eq!(distinct_speakers(&out_i), 2);

        // In both runs the A's group together and the B's group together —
        // the *partition* is identical even though arrival order differs.
        assert_eq!(out_f[0].speaker, out_f[1].speaker); // a1 == a2
        assert_eq!(out_f[2].speaker, out_f[3].speaker); // b1 == b2
        assert_eq!(out_i[1].speaker, out_i[3].speaker); // a1 == a2
        assert_eq!(out_i[0].speaker, out_i[2].speaker); // b1 == b2
        assert_ne!(out_i[0].speaker, out_i[1].speaker); // A != B
    }

    /// A voice the live pass stranded under its own label gets folded back
    /// into the cluster it belongs to — something re-running the greedy pass
    /// in the same order could never fix.
    #[test]
    fn live_mis_split_is_repaired_and_marked_changed() {
        // All three embeddings are the same voice, but the live pass tagged
        // the middle one "Speaker 2" (warmup noise / unstable centroid).
        let history = vec![
            rec(1, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(2, vec![0.99, 0.01, 0.0], "Speaker 2"),
            rec(3, vec![0.98, 0.02, 0.0], "Speaker 1"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 1);
        assert_eq!(out[0].speaker, "Speaker 1");
        assert_eq!(out[1].speaker, "Speaker 1");
        // The repaired row is flagged, the already-correct ones are not.
        assert!(out[1].changed);
        assert!(!out[0].changed);
        assert_eq!(out[1].previous_speaker, "Speaker 2");
    }

    /// Labels are assigned by first appearance, so the first voice heard is
    /// always "Speaker 1" regardless of internal HAC cluster indices.
    #[test]
    fn labels_follow_first_appearance_order() {
        let history = vec![
            rec(1, vec![0.0, 1.0, 0.0], "Speaker 6"),
            rec(2, vec![1.0, 0.0, 0.0], "Speaker 10"),
            rec(3, vec![0.0, 0.98, 0.02], "Speaker 6"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(out[0].speaker, "Speaker 1");
        assert_eq!(out[1].speaker, "Speaker 2");
        assert_eq!(out[2].speaker, "Speaker 1");
    }

    #[test]
    fn single_unpinned_record_is_speaker_one() {
        let out = refine(
            &[rec(1, vec![1.0, 0.0, 0.0], "Speaker 4")],
            &RefineOptions::default(),
            None,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].speaker, "Speaker 1");
        assert!(out[0].changed);
    }

    #[test]
    fn every_assignment_preserves_its_sequence_id() {
        let history = vec![
            rec(41, vec![1.0, 0.0, 0.0], "Speaker 1"),
            rec(42, vec![0.0, 1.0, 0.0], "Speaker 2"),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(out[0].sequence_id, 41);
        assert_eq!(out[1].sequence_id, 42);
    }

    // ── #102: folding, same-voice merge, channels, head-count cap ─────────

    /// A record with an explicit channel and length.
    fn seg(seq: u64, emb: Vec<f32>, source: SegmentSource, secs: f32) -> EmbeddingRecord {
        EmbeddingRecord {
            duration_secs: secs,
            source,
            ..rec(seq, emb, &format!("Speaker {seq}"))
        }
    }

    /// Unit-ish vector in `dim` dimensions from sparse `(index, value)` pairs.
    fn v(dim: usize, parts: &[(usize, f32)]) -> Vec<f32> {
        let mut out = vec![0.0; dim];
        for &(i, x) in parts {
            out[i] = x;
        }
        l2_normalize(&mut out);
        out
    }

    fn label_of(out: &[RefinedAssignment], seq: u64) -> &str {
        &out.iter().find(|a| a.sequence_id == seq).unwrap().speaker
    }

    #[test]
    fn short_lived_cluster_folds_into_the_nearest_substantial_voice() {
        // A: three 30 s segments. The stray: one 5 s segment about 0.5 alike
        // to A (below the HAC cut, above the fold floor).
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(2, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(3, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(4, v(3, &[(0, 1.0), (1, 1.7)]), SegmentSource::System, 5.0),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 1);
        assert_eq!(label_of(&out, 4), label_of(&out, 1));
        // The legacy pass leaves it stranded.
        let legacy = refine(&history, &RefineOptions::legacy(), None);
        assert_eq!(distinct_speakers(&legacy), 2);
    }

    #[test]
    fn a_short_voice_unlike_anyone_is_not_folded() {
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(2, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            // 6 s, orthogonal: a different person who said one thing.
            seg(3, v(3, &[(1, 1.0)]), SegmentSource::System, 6.0),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 2);
        assert_ne!(label_of(&out, 3), label_of(&out, 1));
    }

    #[test]
    fn two_substantial_voices_are_never_folded_together() {
        // 0.4 alike: below the same-voice merge, and neither is minor.
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(
                2,
                v(3, &[(0, 0.4), (1, 0.9165)]),
                SegmentSource::System,
                30.0,
            ),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 2);
    }

    #[test]
    fn sub_second_fragments_go_to_the_nearest_voice_on_their_channel() {
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            // Noise-like embedding, but only 0.6 s long.
            seg(2, v(3, &[(2, 1.0)]), SegmentSource::System, 0.6),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(label_of(&out, 2), label_of(&out, 1));
    }

    /// Two halves of one voice: every segment carries its own noise, so
    /// pairwise (average-linkage) similarity across the halves sits below
    /// the HAC cut, but the half-centroids — which average the noise away —
    /// are clearly the same voice.
    #[test]
    fn halves_of_one_voice_merge_by_centroid() {
        // Record = base e0 + 0.5·(half axis) + 0.95·(own noise axis).
        // Within a half: ≈0.58 (HAC joins), across: ≈0.46 (HAC doesn't),
        // half-centroids of 8: ≈0.73.
        let dim = 3 + 16;
        let mut history = Vec::new();
        for i in 0..16usize {
            let half_axis = if i < 8 { 1 } else { 2 };
            let e = v(dim, &[(0, 1.0), (half_axis, 0.5), (3 + i, 0.95)]);
            history.push(seg(i as u64, e, SegmentSource::System, 10.0));
        }
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 1);

        let no_merge = RefineOptions {
            merge_similarity: 1.1,
            mic_merge_similarity: 1.1,
            ..RefineOptions::default()
        };
        assert_eq!(distinct_speakers(&refine(&history, &no_merge, None)), 2);
    }

    #[test]
    fn identical_voices_on_different_channels_stay_apart() {
        // The far end can't be in the user's room: the mic and system
        // channels never share a cluster.
        let voice = v(3, &[(0, 1.0)]);
        let history = vec![
            seg(1, voice.clone(), SegmentSource::Microphone, 30.0),
            seg(2, voice.clone(), SegmentSource::System, 30.0),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_ne!(label_of(&out, 1), label_of(&out, 2));
        // Without channel information they're one voice, as before.
        let legacy = refine(&history, &RefineOptions::legacy(), None);
        assert_eq!(distinct_speakers(&legacy), 1);
    }

    #[test]
    fn a_few_seconds_on_the_mic_never_spawn_a_new_speaker() {
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::Microphone, 60.0),
            seg(2, v(3, &[(0, 1.0)]), SegmentSource::Microphone, 60.0),
            // Unlike the user (orthogonal), but only 8 s on their channel.
            seg(3, v(3, &[(1, 1.0)]), SegmentSource::Microphone, 8.0),
            // The far end, same embedding as the stray: stays its own voice.
            seg(4, v(3, &[(1, 1.0)]), SegmentSource::System, 30.0),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(label_of(&out, 3), label_of(&out, 1));
        assert_ne!(label_of(&out, 4), label_of(&out, 1));
        assert_eq!(distinct_speakers(&out), 2);
    }

    #[test]
    fn unclear_channel_segments_join_a_channel_voice() {
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::Microphone, 60.0),
            seg(2, v(3, &[(1, 1.0)]), SegmentSource::System, 60.0),
            // Crosstalk, 30 s, mostly the system voice.
            seg(3, v(3, &[(0, 0.5), (1, 1.0)]), SegmentSource::Unknown, 30.0),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 2);
        assert_eq!(label_of(&out, 3), label_of(&out, 2));
    }

    #[test]
    fn attendee_cap_merges_the_closest_unknown_voices_first() {
        // Three substantial voices; 1 and 2 are 0.5 alike, 3 is unlike both.
        // The default floor keeps 0.5 apart (see the next test), so exercise
        // the mechanism with a lower one.
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(
                2,
                v(3, &[(0, 0.5), (1, 0.866)]),
                SegmentSource::System,
                30.0,
            ),
            seg(3, v(3, &[(2, 1.0)]), SegmentSource::System, 30.0),
        ];
        let low_floor = RefineOptions {
            cap_similarity_floor: 0.40,
            ..RefineOptions::default()
        };
        assert_eq!(distinct_speakers(&refine(&history, &low_floor, None)), 3);

        let out = refine(
            &history,
            &low_floor.clone().with_max_speakers(Some(2)),
            None,
        );
        assert_eq!(distinct_speakers(&out), 2);
        assert_eq!(label_of(&out, 1), label_of(&out, 2));

        // The cap is soft: it never fuses voices below the similarity floor.
        let out = refine(&history, &low_floor.with_max_speakers(Some(1)), None);
        assert_eq!(distinct_speakers(&out), 2);
    }

    #[test]
    fn attendee_cap_keeps_probably_different_people_apart() {
        // 0.5 alike: in #102's data different people sat up to ~0.42 and one
        // person's two profiles at 0.57, so the attendee list alone mustn't
        // fuse these (that's how one "Speaker 3" held two people).
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(
                2,
                v(3, &[(0, 0.5), (1, 0.866)]),
                SegmentSource::System,
                30.0,
            ),
            seg(3, v(3, &[(2, 1.0)]), SegmentSource::System, 30.0),
        ];
        let out = refine(
            &history,
            &RefineOptions::default().with_max_speakers(Some(2)),
            None,
        );
        assert_eq!(distinct_speakers(&out), 3);
    }

    #[test]
    fn a_quiet_voice_is_kept_while_an_invitee_is_unaccounted_for() {
        // One substantial voice and a 6 s stray 0.40 alike to it: above the
        // usual fold bar, below the missing-attendee one.
        let history = vec![
            seg(1, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(2, v(3, &[(0, 1.0)]), SegmentSource::System, 30.0),
            seg(3, v(3, &[(0, 1.0), (1, 2.3)]), SegmentSource::System, 6.0),
        ];
        // No head count: folded, as before.
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(distinct_speakers(&out), 1);
        // Two expected, one heard: the stray may be the second invitee.
        let out = refine(
            &history,
            &RefineOptions::default().with_max_speakers(Some(2)),
            None,
        );
        assert_eq!(distinct_speakers(&out), 2);
        // Everyone already accounted for: folded.
        let out = refine(
            &history,
            &RefineOptions::default().with_max_speakers(Some(1)),
            None,
        );
        assert_eq!(distinct_speakers(&out), 1);
    }

    #[test]
    fn attendee_cap_never_fuses_two_named_people() {
        let history = vec![
            pinned(1, v(3, &[(0, 1.0)]), "John", "p-john"),
            pinned(2, v(3, &[(0, 0.6), (1, 0.8)]), "Mary", "p-mary"),
        ];
        let out = refine(
            &history,
            &RefineOptions::default().with_max_speakers(Some(1)),
            None,
        );
        assert_eq!(label_of(&out, 1), "John");
        assert_eq!(label_of(&out, 2), "Mary");
    }

    fn self_matcher(self_voice: Vec<f32>) -> Option<SpeakerProfileMatcher> {
        let dim = self_voice.len();
        match SpeakerProfileMatcher::new(
            dim,
            vec![("p-self".to_string(), "Seb".to_string(), self_voice)],
            0.60,
        ) {
            Ok(m) => Some(m.with_self_profile(Some("p-self".to_string()))),
            Err(e) => {
                eprintln!("skipping: SpeakerEmbeddingManager unavailable ({})", e);
                None
            }
        }
    }

    #[test]
    fn a_weak_self_match_on_the_system_channel_is_not_the_user() {
        let Some(matcher) = self_matcher(v(3, &[(0, 1.0)])) else {
            return;
        };
        let mut far_end = seg(
            1,
            v(3, &[(0, 0.62), (1, 0.785)]),
            SegmentSource::System,
            30.0,
        );
        far_end.voice_profile_id = Some("p-self".into());
        far_end.label = "Seb".into();
        far_end.profile_score = Some(0.62);
        let mut strong = seg(
            2,
            v(3, &[(0, 0.9), (1, -0.436)]),
            SegmentSource::System,
            30.0,
        );
        strong.voice_profile_id = Some("p-self".into());
        strong.label = "Seb".into();
        strong.profile_score = Some(0.9);

        let out = refine(
            &[far_end, strong],
            &RefineOptions::default(),
            Some(&matcher),
        );
        assert_ne!(label_of(&out, 1), "Seb");
        assert!(out[0].voice_profile_id.is_none());
        assert!(out[0].changed);
        // A strong match stays pinned even on the system channel.
        assert_eq!(label_of(&out, 2), "Seb");
        assert!(!out[1].changed);
    }

    #[test]
    fn a_mic_voice_reasonably_like_the_user_is_the_user() {
        let Some(matcher) = self_matcher(v(3, &[(0, 1.0)])) else {
            return;
        };
        // 0.5 alike to the enrolled voice: under the 0.60 profile match, but
        // it's on the user's own channel.
        let mic = v(3, &[(0, 0.5), (1, 0.866)]);
        let history = vec![
            seg(1, mic.clone(), SegmentSource::Microphone, 30.0),
            seg(2, mic.clone(), SegmentSource::Microphone, 30.0),
            // The same voice on the system channel is *not* taken as the user.
            seg(3, mic.clone(), SegmentSource::System, 30.0),
        ];
        let out = refine(&history, &RefineOptions::default(), Some(&matcher));
        assert_eq!(label_of(&out, 1), "Seb");
        assert_eq!(out[0].voice_profile_id.as_deref(), Some("p-self"));
        assert_eq!(label_of(&out, 3), "Speaker 1");
    }

    #[test]
    fn mic_clusters_merge_at_a_lower_floor_than_system_ones() {
        // Two pairs of substantial clusters, each pair 0.45 alike: above the
        // mic floor (0.40), below the general one (0.60).
        let a = v(3, &[(0, 1.0)]);
        let b = v(3, &[(0, 0.45), (1, 0.893)]);
        let history = vec![
            seg(1, a.clone(), SegmentSource::Microphone, 30.0),
            seg(2, b.clone(), SegmentSource::Microphone, 30.0),
            seg(3, a, SegmentSource::System, 30.0),
            seg(4, b, SegmentSource::System, 30.0),
        ];
        let out = refine(&history, &RefineOptions::default(), None);
        assert_eq!(label_of(&out, 1), label_of(&out, 2));
        assert_ne!(label_of(&out, 3), label_of(&out, 4));
    }

    /// A duplicate profile named like the user (made by naming a chip after
    /// themselves) is held to the same system-channel rule as the real self
    /// profile — scored against the real one, since the duplicate may have
    /// absorbed other voices.
    #[test]
    fn a_same_named_duplicate_of_the_user_counts_as_self() {
        let self_voice = v(3, &[(0, 1.0)]);
        let contaminated = v(3, &[(1, 1.0)]);
        let matcher = match SpeakerProfileMatcher::new(
            3,
            vec![
                ("p-self".to_string(), "Seb".to_string(), self_voice),
                (
                    "p-dup".to_string(),
                    " seb".to_string(),
                    contaminated.clone(),
                ),
            ],
            0.60,
        ) {
            Ok(m) => m.with_self_profile(Some("p-self".to_string())),
            Err(e) => {
                eprintln!("skipping: SpeakerEmbeddingManager unavailable ({})", e);
                return;
            }
        };
        assert!(matcher.is_self("p-dup"));

        // The far end matched the duplicate strongly (0.95), but sounds
        // nothing like the real self profile.
        let mut far_end = seg(1, contaminated, SegmentSource::System, 30.0);
        far_end.voice_profile_id = Some("p-dup".into());
        far_end.label = " seb".into();
        far_end.profile_score = Some(0.95);
        let out = refine(&[far_end], &RefineOptions::default(), Some(&matcher));
        assert_eq!(out[0].speaker, "Speaker 1");
        assert!(out[0].voice_profile_id.is_none());
    }
}
