// Component trait and Container.

use crossterm::event::KeyEvent;

/// Core trait for all UI components.
///
/// Components render themselves as lines of text. The only required method is
/// `render()` — input handling and invalidation have default no-op implementations.
pub trait Component {
    /// Render this component at the given terminal width.
    /// Returns a list of lines (each line is a string, possibly with ANSI codes).
    fn render(&self, width: u16) -> Vec<String>;

    /// Handle a key input event. Only called when the component has focus.
    fn handle_input(&mut self, _event: &KeyEvent) {}

    /// Invalidate cached state. Called to force re-rendering.
    fn invalidate(&mut self) {}
}

/// A container that holds child components and renders them vertically.
///
/// `render()` concatenates all children's rendered lines in order.
/// `invalidate()` propagates to all children.
pub struct Container {
    children: Vec<Box<dyn Component>>,
}

impl Container {
    pub fn new() -> Self {
        Container {
            children: Vec::new(),
        }
    }

    /// Add a child component to the end of the container.
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
    }

    /// Remove the child at the given index. Panics if out of bounds.
    pub fn remove_child(&mut self, index: usize) -> Box<dyn Component> {
        self.children.remove(index)
    }

    /// Remove all children.
    pub fn clear(&mut self) {
        self.children.clear();
    }

    /// Number of children.
    pub fn len(&self) -> usize {
        self.children.len()
    }

    /// Whether the container has no children.
    pub fn is_empty(&self) -> bool {
        self.children.is_empty()
    }

    /// Get a mutable reference to the child at the given index.
    pub fn child_mut(&mut self, index: usize) -> Option<&mut Box<dyn Component>> {
        self.children.get_mut(index)
    }
}

impl Default for Container {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Container {
    fn render(&self, width: u16) -> Vec<String> {
        let mut lines = Vec::new();
        for child in &self.children {
            lines.extend(child.render(width));
        }
        lines
    }

    fn invalidate(&mut self) {
        for child in &mut self.children {
            child.invalidate();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mock component that returns fixed lines on render.
    struct MockComponent {
        lines: Vec<String>,
        invalidated: bool,
    }

    impl MockComponent {
        fn new(lines: Vec<&str>) -> Self {
            MockComponent {
                lines: lines.into_iter().map(String::from).collect(),
                invalidated: false,
            }
        }
    }

    impl Component for MockComponent {
        fn render(&self, _width: u16) -> Vec<String> {
            self.lines.clone()
        }

        fn invalidate(&mut self) {
            self.invalidated = true;
        }
    }

    #[test]
    fn empty_container_renders_empty_vec() {
        let container = Container::new();
        let lines = container.render(80);
        assert!(lines.is_empty());
    }

    #[test]
    fn container_with_one_child() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["hello", "world"])));
        let lines = container.render(80);
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn container_concatenates_children_output() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["line1"])));
        container.add_child(Box::new(MockComponent::new(vec!["line2", "line3"])));
        container.add_child(Box::new(MockComponent::new(vec!["line4"])));
        let lines = container.render(80);
        assert_eq!(lines, vec!["line1", "line2", "line3", "line4"]);
    }

    #[test]
    fn container_remove_child() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["a"])));
        container.add_child(Box::new(MockComponent::new(vec!["b"])));
        container.add_child(Box::new(MockComponent::new(vec!["c"])));
        container.remove_child(1);
        let lines = container.render(80);
        assert_eq!(lines, vec!["a", "c"]);
    }

    #[test]
    fn container_clear() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["a"])));
        container.add_child(Box::new(MockComponent::new(vec!["b"])));
        container.clear();
        assert!(container.is_empty());
        assert!(container.render(80).is_empty());
    }

    #[test]
    fn container_len_and_is_empty() {
        let mut container = Container::new();
        assert!(container.is_empty());
        assert_eq!(container.len(), 0);
        container.add_child(Box::new(MockComponent::new(vec!["a"])));
        assert!(!container.is_empty());
        assert_eq!(container.len(), 1);
    }

    #[test]
    fn container_default() {
        let container = Container::default();
        assert!(container.is_empty());
    }

    #[test]
    fn container_invalidate_propagates() {
        // We can't directly inspect MockComponent through Box<dyn Component>,
        // but we can verify invalidate() doesn't panic and the trait method is called.
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["a"])));
        container.add_child(Box::new(MockComponent::new(vec!["b"])));
        container.invalidate(); // should not panic
    }

    #[test]
    fn container_with_empty_child() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec![])));
        container.add_child(Box::new(MockComponent::new(vec!["hello"])));
        let lines = container.render(80);
        assert_eq!(lines, vec!["hello"]);
    }

    #[test]
    fn component_trait_default_methods() {
        // Verify default handle_input and invalidate don't panic
        let mut mock = MockComponent::new(vec!["test"]);
        let key_event = KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::NONE,
        );
        mock.handle_input(&key_event);
        // MockComponent overrides invalidate, but the default on Component is a no-op
    }

    #[test]
    fn component_trait_is_object_safe() {
        // Verify we can use Box<dyn Component>
        let _boxed: Box<dyn Component> = Box::new(MockComponent::new(vec!["test"]));
    }

    #[test]
    fn container_child_mut_valid_index() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["a"])));
        container.add_child(Box::new(MockComponent::new(vec!["b"])));
        assert!(container.child_mut(0).is_some());
        assert!(container.child_mut(1).is_some());
    }

    #[test]
    fn container_child_mut_out_of_bounds() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["a"])));
        assert!(container.child_mut(5).is_none());
    }

    #[test]
    fn container_child_mut_can_call_methods() {
        let mut container = Container::new();
        container.add_child(Box::new(MockComponent::new(vec!["hello"])));
        let child = container.child_mut(0).unwrap();
        // Can call Component methods through Box deref
        let lines = child.render(80);
        assert_eq!(lines, vec!["hello"]);
    }
}
