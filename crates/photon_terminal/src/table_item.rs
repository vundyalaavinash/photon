use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Window};
use tabular_data_preview::{TableView, table_contents};
use ui::{SharedString, prelude::*};
use workspace::Item;

/// A workspace tab showing a block's table output in the tabular data viewer.
pub struct TableItem {
    title: SharedString,
    table: Entity<TableView>,
}

impl TableItem {
    /// The first row is the header.
    pub fn new(
        title: SharedString,
        rows: Vec<Vec<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let table = cx.new(|cx| TableView::new(window, cx));
        table.update(cx, |table, cx| table.set_contents(table_contents(rows), cx));
        Self { title, table }
    }

    pub fn table(&self) -> &Entity<TableView> {
        &self.table
    }
}

impl EventEmitter<()> for TableItem {}

impl Focusable for TableItem {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.table.read(cx).focus_handle(cx)
    }
}

impl Render for TableItem {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.table.clone())
    }
}

impl Item for TableItem {
    type Event = ();

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Table))
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title.clone()
    }
}
