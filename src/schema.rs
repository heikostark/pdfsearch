use tantivy::schema::{Schema, STORED, STRING, TEXT};

/// Index field handles, bundled in one place so `indexer.rs` and
/// `search.rs` are guaranteed to use the same schema.
pub struct Fields {
    pub path: tantivy::schema::Field,
    pub filename: tantivy::schema::Field,
    pub page: tantivy::schema::Field,
    pub content: tantivy::schema::Field,
}

pub fn build() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    // STRING = indexed but NOT tokenized -> exact match, needed so that
    // re-indexing can precisely delete_term() all pages of one specific
    // file.
    let path = builder.add_text_field("path", STRING | STORED);
    let filename = builder.add_text_field("filename", TEXT | STORED);
    let page = builder.add_u64_field("page", STORED);
    // STORED, so the snippet generator can build a highlighted context
    // excerpt from the stored field value.
    let content = builder.add_text_field("content", TEXT | STORED);
    let schema = builder.build();
    (
        schema,
        Fields {
            path,
            filename,
            page,
            content,
        },
    )
}
