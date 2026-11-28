import "ag-grid-community/styles/ag-grid.css";
import "ag-grid-community/styles/ag-theme-quartz.css";
import "./index.css";

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  CellStyleModule,
  ClientSideRowModelApiModule,
  ClientSideRowModelModule,
  HighlightChangesModule,
  ModuleRegistry,
  NumberFilterModule,
  RenderApiModule,
  RowAutoHeightModule,
  RowStyleModule,
  TextFilterModule,
  TooltipModule,
} from "ag-grid-community";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { Store } from "./store";

// Only what the page uses: client-side rows and their transactions, text and number
// filters, the highlight of a changed value, wrapped rows, cell and row classes, tooltips,
// and refreshing cells (times shown from now).
ModuleRegistry.registerModules([
  RenderApiModule,
  RowStyleModule,
  TooltipModule,
  ClientSideRowModelModule,
  ClientSideRowModelApiModule,
  TextFilterModule,
  NumberFilterModule,
  HighlightChangesModule,
  RowAutoHeightModule,
  CellStyleModule,
]);

const store = new Store();
const client = new QueryClient({ defaultOptions: { queries: { retry: 2 } } });

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={client}>
      <App store={store} />
    </QueryClientProvider>
  </StrictMode>,
);
