import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import App from './App.svelte';

describe('App', () => {
  it('renders the heading', () => {
    render(App);
    expect(screen.getByText('rio-dashboard')).toBeInTheDocument();
  });
});
