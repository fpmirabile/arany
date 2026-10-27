use super::*;

#[cfg(test)]
mod tests;

impl Capture<'_> {
    pub(super) fn remove_subtrees(&mut self) -> Result<(), ToolError> {
        let mut roots = BTreeSet::<String>::new();
        for path in std::mem::take(&mut self.subtrees) {
            self.selector.check_time()?;
            if !Path::new(&path)
                .ancestors()
                .filter_map(Path::to_str)
                .any(|ancestor| roots.contains(ancestor))
            {
                roots.insert(path);
            }
        }
        self.subtrees = roots;
        self.check_subtrees()?;
        for root in &self.subtrees {
            for (path, metadata) in descendants(self.baseline, root) {
                self.selector.check_time()?;
                self.totals.budget.entry(&joined(self.prefix, path))?;
                if !metadata.is_file()
                    || self.files.contains_key(path)
                    || self.discarded.contains(path)
                {
                    continue;
                }
                if ignored(&joined(self.prefix, path), false, self.selector)? {
                    self.totals.summary.discarded_outputs += 1;
                } else {
                    self.files.insert(path.clone(), Output::Deletion);
                }
            }
        }
        Ok(())
    }

    pub(super) fn check_subtrees(&mut self) -> Result<(), ToolError> {
        let roots = std::mem::take(&mut self.subtrees);
        let baseline = self.baseline;
        for root in &roots {
            self.selector.check_time()?;
            if self
                .baseline
                .masks
                .iter()
                .any(|mask| Path::new(mask).starts_with(root))
            {
                return Err(ToolError::Conflict);
            }
            if !root.is_empty() {
                self.baseline.check(self.lower, root)?;
                match self.baseline.entries.get(root) {
                    None => continue,
                    Some(metadata) if metadata.is_dir() => {}
                    Some(_) => return Err(ToolError::Path),
                }
            }
            self.check_directory(root)?;
            for (path, metadata) in descendants(baseline, root) {
                self.selector.check_time()?;
                self.baseline.check(self.lower, path)?;
                if metadata.is_dir() {
                    self.check_directory(path)?;
                }
            }
        }
        self.subtrees = roots;
        Ok(())
    }

    fn check_directory(&mut self, path: &str) -> Result<(), ToolError> {
        if !self.baseline.complete.contains(path) {
            return Err(ToolError::Conflict);
        }
        let directory = if path.is_empty() {
            self.lower.try_clone().map_err(|_| ToolError::Conflict)?
        } else {
            Dir::from_std_file(open_file(self.lower, path)?)
        };
        for entry in directory.entries().map_err(|_| ToolError::Conflict)? {
            self.selector.check_time()?;
            let name = entry
                .map_err(|_| ToolError::Conflict)?
                .file_name()
                .into_string()
                .map_err(|_| ToolError::Conflict)?;
            let child = joined(path, &name);
            self.totals.budget.entry(&joined(self.prefix, &child))?;
            if !self.baseline.entries.contains_key(&child) {
                return Err(ToolError::Conflict);
            }
            self.baseline.check(self.lower, &child)?;
        }
        Ok(())
    }
}

fn descendants<'a>(
    baseline: &'a Baseline,
    root: &str,
) -> impl Iterator<Item = (&'a String, &'a Metadata)> {
    let prefix = if root.is_empty() {
        String::new()
    } else {
        format!("{root}/")
    };
    baseline
        .entries
        .range(prefix.clone()..)
        .take_while(move |(path, _)| path.starts_with(&prefix))
}
